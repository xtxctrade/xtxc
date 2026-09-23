// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice StockMesh's atomic Monad lane. Issuer-async orders cannot enter it.
/// A listed venue is a reviewed typed adapter, not arbitrary target/calldata.
interface IStockMeshAtomicVenue {
    function swapExactIn(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address recipient
    ) external returns (uint256 amountOut);
}

interface IStockMeshToken {
    function balanceOf(address owner) external view returns (uint256);
    function allowance(address owner, address spender) external view returns (uint256);
}

contract StockMeshExecutor {
    error InvalidOrder();
    error UnadmittedRoute();
    error Expired();
    error UsedNonce();
    error TokenCallFailed();
    error BalanceMismatch();
    error OutputBelowMinimum();
    error FeeAboveCap();
    error Reentered();
    error OnlyGovernor();

    // Bound at deployment: production Monad and the separately labeled
    // Metropolis testnet can use the same execution semantics and test vectors.
    uint256 public immutable CHAIN_ID;
    uint256 public constant FEE_DENOMINATOR = 20_000; // 0.5 bps
    address public immutable usdc;
    address public immutable governor;
    address public immutable feeRecipient;

    mapping(bytes32 => address) public productStock;
    mapping(bytes32 => uint64) public productVersion;
    mapping(bytes32 => mapping(address => bool)) public venueEnabled;
    mapping(bytes32 => mapping(address => uint64)) public venueVersion;
    mapping(bytes32 => mapping(address => bytes32)) public venueCodeHash;
    mapping(address => mapping(uint256 => bool)) public nonceUsed;
    uint256 private entered;

    struct Order {
        bytes32 productId;
        bytes32 quoteDigest;
        address owner;
        address receiver;
        address stock;
        address venue;
        uint256 nonce;
        uint256 amountIn; // BUY: total USDC debit; SELL: stock debit
        uint256 maxDebit;
        uint256 minOutput; // net token out after platform fee
        uint256 feeCap; // USDC atoms
        uint256 deadline;
        bool buy;
    }

    event ProductConfigured(bytes32 indexed productId, address indexed stock);
    event VenueConfigured(bytes32 indexed productId, address indexed venue, bool enabled);
    event Executed(
        bytes32 indexed productId,
        address indexed owner,
        uint256 indexed nonce,
        bytes32 quoteDigest,
        address venue,
        bool buy,
        uint256 walletDebit,
        uint256 venueDebit,
        uint256 walletOutput,
        uint256 platformFee
    );

    constructor(address usdc_, address feeRecipient_) {
        if ((block.chainid != 143 && block.chainid != 10143)
            || usdc_ == address(0) || usdc_.code.length == 0
            || feeRecipient_ == address(0)) revert InvalidOrder();
        CHAIN_ID = block.chainid;
        usdc = usdc_;
        feeRecipient = feeRecipient_;
        governor = msg.sender;
    }

    modifier onlyGovernor() {
        if (msg.sender != governor) revert OnlyGovernor();
        _;
    }

    modifier nonReentrant() {
        if (entered != 0) revert Reentered();
        entered = 1;
        _;
        entered = 0;
    }

    /// @dev Product admission must follow external issuer-rights, ABI and code review.
    /// A product is disabled by assigning stock=address(0). Every stock-pin
    /// change invalidates all prior venue admissions until explicitly renewed.
    function configureProduct(bytes32 productId, address stock) external onlyGovernor {
        if (productId == bytes32(0) || (stock != address(0) && (stock == usdc || stock.code.length == 0)))
            revert InvalidOrder();
        if (productStock[productId] == stock) revert InvalidOrder();
        productVersion[productId] += 1;
        productStock[productId] = stock;
        emit ProductConfigured(productId, stock);
    }

    function configureVenue(bytes32 productId, address venue, bool enabled) external onlyGovernor {
        if (productStock[productId] == address(0) || venue == address(0)
            || (enabled && venue.code.length == 0)) revert InvalidOrder();
        venueEnabled[productId][venue] = enabled;
        venueVersion[productId][venue] = productVersion[productId];
        venueCodeHash[productId][venue] = enabled ? venue.codehash : bytes32(0);
        emit VenueConfigured(productId, venue, enabled);
    }

    /// @notice Wallet-signed transaction only. No delegated signature or
    /// permit path exists in this version. Failure of the single venue reverts
    /// the entire transaction, including nonce and token movements.
    function execute(Order calldata o) external nonReentrant returns (uint256 netOutput) {
        if (block.chainid != CHAIN_ID || msg.sender != o.owner || o.receiver != o.owner
            || o.owner == address(0) || o.productId == bytes32(0) || o.quoteDigest == bytes32(0)
            || o.stock == address(0) || o.venue == address(0)
            || o.amountIn == 0 || o.amountIn > o.maxDebit || o.minOutput == 0) revert InvalidOrder();
        if (block.timestamp > o.deadline) revert Expired();
        if (nonceUsed[o.owner][o.nonce]) revert UsedNonce();
        if (productStock[o.productId] != o.stock || !venueEnabled[o.productId][o.venue]
            || venueVersion[o.productId][o.venue] != productVersion[o.productId]
            || o.venue.code.length == 0 || o.venue.codehash != venueCodeHash[o.productId][o.venue])
            revert UnadmittedRoute();

        address tokenIn = o.buy ? usdc : o.stock;
        address tokenOut = o.buy ? o.stock : usdc;
        uint256 inBefore = IStockMeshToken(tokenIn).balanceOf(address(this));
        uint256 outBefore = IStockMeshToken(tokenOut).balanceOf(address(this));
        nonceUsed[o.owner][o.nonce] = true;
        _safeTransferFrom(tokenIn, o.owner, address(this), o.amountIn);
        if (IStockMeshToken(tokenIn).balanceOf(address(this)) != inBefore + o.amountIn)
            revert BalanceMismatch(); // no fee-on-transfer input

        uint256 buyFee = o.buy ? o.amountIn / FEE_DENOMINATOR : 0;
        if (buyFee > o.feeCap) revert FeeAboveCap();
        uint256 venueDebit = o.amountIn - buyFee;
        if (venueDebit == 0) revert InvalidOrder();
        _safeApprove(tokenIn, o.venue, 0);
        _safeApprove(tokenIn, o.venue, venueDebit);
        IStockMeshAtomicVenue(o.venue).swapExactIn(
            tokenIn, tokenOut, venueDebit, o.minOutput, address(this)
        );
        _safeApprove(tokenIn, o.venue, 0);
        if (IStockMeshToken(tokenIn).allowance(address(this), o.venue) != 0
            || IStockMeshToken(tokenIn).balanceOf(address(this)) != inBefore + buyFee)
            revert BalanceMismatch(); // exact-in; no partial-fill or input dust use

        uint256 outAfter = IStockMeshToken(tokenOut).balanceOf(address(this));
        if (outAfter <= outBefore) revert OutputBelowMinimum();
        uint256 grossOutput = outAfter - outBefore;
        uint256 platformFee = o.buy ? buyFee : grossOutput / FEE_DENOMINATOR;
        if (platformFee > o.feeCap) revert FeeAboveCap();
        netOutput = o.buy ? grossOutput : grossOutput - platformFee;
        if (netOutput < o.minOutput) revert OutputBelowMinimum();

        // The executor sends only this transaction's measured deltas. Any
        // accidental old balance remains untouched and cannot satisfy minOut.
        uint256 walletBefore = IStockMeshToken(tokenOut).balanceOf(o.receiver);
        _safeTransfer(tokenOut, o.receiver, netOutput);
        if (IStockMeshToken(tokenOut).balanceOf(o.receiver) < walletBefore + netOutput)
            revert BalanceMismatch(); // no fee-on-transfer output
        if (platformFee != 0) _safeTransfer(usdc, feeRecipient, platformFee);
        if (IStockMeshToken(tokenIn).balanceOf(address(this)) != inBefore
            || IStockMeshToken(tokenOut).balanceOf(address(this)) != outBefore)
            revert BalanceMismatch();

        emit Executed(o.productId, o.owner, o.nonce, o.quoteDigest, o.venue,
            o.buy, o.amountIn, venueDebit, netOutput, platformFee);
    }

    function _safeTransferFrom(address token, address from, address to, uint256 amount) private {
        _tokenCall(token, abi.encodeWithSelector(0x23b872dd, from, to, amount));
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        _tokenCall(token, abi.encodeWithSelector(0xa9059cbb, to, amount));
    }

    function _safeApprove(address token, address spender, uint256 amount) private {
        _tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, amount));
    }

    function _tokenCall(address token, bytes memory data) private {
        (bool ok, bytes memory response) = token.call(data);
        if (!ok || (response.length != 0 && (response.length != 32 || !abi.decode(response, (bool)))))
            revert TokenCallFailed();
    }
}
