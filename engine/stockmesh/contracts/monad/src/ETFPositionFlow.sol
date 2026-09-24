// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IETFPositionToken {
    function balanceOf(address owner) external view returns (uint256);
    function allowance(address owner, address spender) external view returns (uint256);
}

interface IETFPositionFactory {
    function isVault(address vault) external view returns (bool);
    function admittedAssetCodeHash(address asset) external view returns (bytes32);
}

interface IETFPositionVault is IETFPositionToken {
    function factory() external view returns (address);
    function definitionHash() external view returns (bytes32);
    function assetCount() external view returns (uint256);
    function assetAt(uint256 index) external view returns (address, uint256, bytes32);
    function previewClaim(uint256 shares) external view returns (uint256[] memory);
    function mint(uint256 shares, address receiver) external;
    function redeem(uint256 shares, address receiver) external;
}

interface IETFPositionVenue {
    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn,
        uint256 minAmountOut, address recipient) external returns (uint256 amountOut);
}

/// @notice Wallet-owned, single-transaction ETF entry/exit. It neither holds
/// balances between calls nor turns a venue callback into an arbitrary call.
/// A non-atomic or issuer-asynchronous route must use a separate staged order.
contract ETFPositionFlow {
    error InvalidOrder();
    error UnadmittedRoute();
    error Expired();
    error UsedNonce();
    error BalanceMismatch();
    error OutputBelowMinimum();
    error FeeAboveCap();
    error TokenCallFailed();
    error Reentered();
    error OnlyGovernor();

    uint256 public constant FEE_DENOMINATOR = 20_000; // 0.5 bps of traded USDC
    uint256 public immutable CHAIN_ID;
    address public immutable usdc;
    address public immutable factory;
    address public immutable feeRecipient;
    address public immutable governor;

    struct Admission { bytes32 codeHash; bytes32 definitionHash; }
    mapping(address => Admission) public vaultAdmission;
    mapping(address => mapping(address => mapping(address => bytes32))) public venueCodeHash;
    mapping(address => mapping(uint256 => bool)) public nonceUsed;
    uint256 private entered;

    struct BuyLeg {
        uint256 fromWallet; // exact stock atoms explicitly authorized by the owner
        uint256 cashIn; // exact USDC atoms spent at this venue
        uint256 minBought; // per-leg price protection, at least the shortage
        address venue; // zero when the wallet supplies the entire component
    }

    struct InvestOrder {
        address vault;
        address owner;
        address receiver;
        uint256 shares;
        uint256 nonce;
        bytes32 quoteDigest;
        uint256 deadline;
        uint256 maxUsdcDebit; // only this amount is pulled, remainder returned
        uint256 feeCap;
        BuyLeg[] legs; // exact vault assetAt order
    }

    struct SellLeg { address venue; uint256 minUsdcOut; }
    struct CashExitOrder {
        address vault;
        address owner;
        address receiver;
        uint256 shares;
        uint256 nonce;
        bytes32 quoteDigest;
        uint256 deadline;
        uint256 minUsdcOut; // total net of platform fee
        uint256 feeCap;
        SellLeg[] legs;
    }

    event VaultConfigured(address indexed vault, bool enabled, bytes32 definitionHash);
    event VenueConfigured(address indexed vault, address indexed asset, address indexed venue, bool enabled);
    event ETFInvested(address indexed vault, address indexed owner, uint256 indexed nonce,
        bytes32 quoteDigest, uint256 shares, uint256 usdcSpent, uint256 fee, uint256 usdcReturned);
    event ETFRedeemed(address indexed vault, address indexed owner, uint256 indexed nonce,
        bytes32 quoteDigest, uint256 shares, bool cashExit, uint256 usdcReceived, uint256 fee);

    constructor(address usdc_, address factory_, address feeRecipient_) {
        if ((block.chainid != 143 && block.chainid != 10143)
            || usdc_ == address(0) || usdc_.code.length == 0
            || factory_ == address(0) || factory_.code.length == 0
            || feeRecipient_ == address(0)) revert InvalidOrder();
        CHAIN_ID = block.chainid;
        usdc = usdc_;
        factory = factory_;
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

    function configureVault(address vault, bool enabled) external onlyGovernor {
        if (vault == address(0)) revert InvalidOrder();
        if (enabled) {
            if (vault.code.length == 0 || !IETFPositionFactory(factory).isVault(vault)
                || IETFPositionVault(vault).factory() != factory) revert UnadmittedRoute();
            vaultAdmission[vault] = Admission(vault.codehash, IETFPositionVault(vault).definitionHash());
        } else {
            delete vaultAdmission[vault];
        }
        emit VaultConfigured(vault, enabled, vaultAdmission[vault].definitionHash);
    }

    function configureVenue(address vault, address asset, address venue, bool enabled) external onlyGovernor {
        if (vaultAdmission[vault].codeHash == bytes32(0) || venue == address(0)
            || (enabled && venue.code.length == 0)) revert InvalidOrder();
        bool found;
        IETFPositionVault v = IETFPositionVault(vault);
        for (uint256 i; i < v.assetCount(); ++i) {
            (address listed,,) = v.assetAt(i);
            if (listed == asset) { found = true; break; }
        }
        if (!found) revert InvalidOrder();
        venueCodeHash[vault][asset][venue] = enabled ? venue.codehash : bytes32(0);
        emit VenueConfigured(vault, asset, venue, enabled);
    }

    function invest(InvestOrder calldata o) external nonReentrant {
        _validate(o.vault, o.owner, o.receiver, o.nonce, o.quoteDigest, o.deadline);
        IETFPositionVault v = IETFPositionVault(o.vault);
        uint256 count = v.assetCount();
        if (o.legs.length != count) revert InvalidOrder();
        uint256[] memory need = v.previewClaim(o.shares);
        uint256[] memory assetBefore = new uint256[](count);
        uint256 usdcBefore = IETFPositionToken(usdc).balanceOf(address(this));
        uint256 spent;
        for (uint256 i; i < count; ++i) {
            (address asset,,) = _asset(v, i, true);
            assetBefore[i] = IETFPositionToken(asset).balanceOf(address(this));
            BuyLeg calldata leg = o.legs[i];
            if (leg.fromWallet > need[i]) revert InvalidOrder();
            if (leg.cashIn == 0) {
                if (leg.fromWallet != need[i] || leg.venue != address(0)
                    || leg.minBought != 0) revert InvalidOrder();
            } else {
                if (leg.venue == address(0) || leg.minBought < need[i] - leg.fromWallet)
                    revert InvalidOrder();
                _venue(o.vault, asset, leg.venue);
                spent += leg.cashIn;
            }
        }
        uint256 fee = spent / FEE_DENOMINATOR;
        if (fee > o.feeCap || spent + fee > o.maxUsdcDebit) revert FeeAboveCap();
        nonceUsed[o.owner][o.nonce] = true;
        if (o.maxUsdcDebit != 0) _pull(usdc, o.owner, o.maxUsdcDebit);
        for (uint256 i; i < count; ++i) {
            (address asset,,) = _asset(v, i, true);
            BuyLeg calldata leg = o.legs[i];
            uint256 beforeAsset = assetBefore[i];
            if (leg.fromWallet != 0) _pull(asset, o.owner, leg.fromWallet);
            if (leg.cashIn != 0) {
                uint256 beforeCash = IETFPositionToken(usdc).balanceOf(address(this));
                _approve(usdc, leg.venue, leg.cashIn);
                IETFPositionVenue(leg.venue).swapExactIn(usdc, asset, leg.cashIn,
                    leg.minBought, address(this));
                _approve(usdc, leg.venue, 0);
                if (IETFPositionToken(usdc).allowance(address(this), leg.venue) != 0
                    || IETFPositionToken(usdc).balanceOf(address(this)) != beforeCash - leg.cashIn)
                    revert BalanceMismatch();
            }
            uint256 acquired = IETFPositionToken(asset).balanceOf(address(this)) - beforeAsset;
            if (acquired < need[i] || acquired < leg.fromWallet + leg.minBought)
                revert OutputBelowMinimum();
            _approve(asset, o.vault, need[i]);
            // The vault checks its own exact reserve delta on mint. Keep all
            // remaining assets local only until this loop has finished.
        }
        uint256 shareBefore = v.balanceOf(o.receiver);
        v.mint(o.shares, o.receiver);
        if (v.balanceOf(o.receiver) != shareBefore + o.shares) revert BalanceMismatch();
        for (uint256 i; i < count; ++i) {
            (address asset,,) = _asset(v, i, true);
            _approve(asset, o.vault, 0);
            if (IETFPositionToken(asset).allowance(address(this), o.vault) != 0)
                revert BalanceMismatch();
            // Return only this order's surplus, never a prior donation.
            uint256 residue = IETFPositionToken(asset).balanceOf(address(this)) - assetBefore[i];
            if (residue != 0) _send(asset, o.owner, residue);
            if (IETFPositionToken(asset).balanceOf(address(this)) != assetBefore[i])
                revert BalanceMismatch();
        }
        if (fee != 0) _send(usdc, feeRecipient, fee);
        uint256 refund = o.maxUsdcDebit - spent - fee;
        if (refund != 0) _send(usdc, o.owner, refund);
        if (IETFPositionToken(usdc).balanceOf(address(this)) != usdcBefore) revert BalanceMismatch();
        emit ETFInvested(o.vault, o.owner, o.nonce, o.quoteDigest, o.shares, spent, fee, refund);
    }

    function redeemInKind(address vault, uint256 shares, uint256 nonce,
        bytes32 quoteDigest, uint256 deadline) external nonReentrant {
        _validate(vault, msg.sender, msg.sender, nonce, quoteDigest, deadline);
        IETFPositionVault v = IETFPositionVault(vault);
        uint256 count = v.assetCount();
        uint256[] memory beforeBalances = new uint256[](count);
        for (uint256 i; i < count; ++i) {
            (address asset,,) = _asset(v, i, false);
            beforeBalances[i] = IETFPositionToken(asset).balanceOf(address(this));
        }
        nonceUsed[msg.sender][nonce] = true;
        uint256 shareBefore = v.balanceOf(address(this));
        _pull(vault, msg.sender, shares);
        v.redeem(shares, msg.sender);
        if (v.balanceOf(address(this)) != shareBefore) revert BalanceMismatch();
        for (uint256 i; i < count; ++i) {
            (address asset,,) = _asset(v, i, false);
            if (IETFPositionToken(asset).balanceOf(address(this)) != beforeBalances[i])
                revert BalanceMismatch();
        }
        emit ETFRedeemed(vault, msg.sender, nonce, quoteDigest, shares, false, 0, 0);
    }

    function redeemToUsdc(CashExitOrder calldata o) external nonReentrant returns (uint256 netUsdc) {
        _validate(o.vault, o.owner, o.receiver, o.nonce, o.quoteDigest, o.deadline);
        IETFPositionVault v = IETFPositionVault(o.vault);
        uint256 count = v.assetCount();
        if (o.legs.length != count || o.minUsdcOut == 0) revert InvalidOrder();
        for (uint256 i; i < count; ++i) {
            (address asset,,) = _asset(v, i, false);
            _venue(o.vault, asset, o.legs[i].venue);
        }
        uint256[] memory claim = v.previewClaim(o.shares);
        uint256 usdcBefore = IETFPositionToken(usdc).balanceOf(address(this));
        nonceUsed[o.owner][o.nonce] = true;
        uint256 shareBefore = v.balanceOf(address(this));
        _pull(o.vault, o.owner, o.shares);
        uint256[] memory assetBefore = new uint256[](count);
        for (uint256 i; i < count; ++i) {
            (address asset,,) = _asset(v, i, false);
            assetBefore[i] = IETFPositionToken(asset).balanceOf(address(this));
        }
        v.redeem(o.shares, address(this));
        if (v.balanceOf(address(this)) != shareBefore) revert BalanceMismatch();
        for (uint256 i; i < count; ++i) {
            (address asset,,) = _asset(v, i, false);
            if (IETFPositionToken(asset).balanceOf(address(this)) != assetBefore[i] + claim[i])
                revert BalanceMismatch();
            uint256 beforeCash = IETFPositionToken(usdc).balanceOf(address(this));
            _approve(asset, o.legs[i].venue, claim[i]);
            IETFPositionVenue(o.legs[i].venue).swapExactIn(asset, usdc, claim[i],
                o.legs[i].minUsdcOut, address(this));
            _approve(asset, o.legs[i].venue, 0);
            if (IETFPositionToken(asset).allowance(address(this), o.legs[i].venue) != 0
                || IETFPositionToken(asset).balanceOf(address(this)) != assetBefore[i]
                || IETFPositionToken(usdc).balanceOf(address(this)) < beforeCash + o.legs[i].minUsdcOut)
                revert BalanceMismatch();
        }
        uint256 gross = IETFPositionToken(usdc).balanceOf(address(this)) - usdcBefore;
        uint256 fee = gross / FEE_DENOMINATOR;
        if (fee > o.feeCap) revert FeeAboveCap();
        netUsdc = gross - fee;
        if (netUsdc < o.minUsdcOut) revert OutputBelowMinimum();
        _send(usdc, o.owner, netUsdc);
        if (fee != 0) _send(usdc, feeRecipient, fee);
        if (IETFPositionToken(usdc).balanceOf(address(this)) != usdcBefore) revert BalanceMismatch();
        emit ETFRedeemed(o.vault, o.owner, o.nonce, o.quoteDigest, o.shares, true, netUsdc, fee);
    }

    function _validate(address vault, address owner, address receiver, uint256 nonce,
        bytes32 digest, uint256 deadline) private view {
        if (block.chainid != CHAIN_ID || msg.sender != owner || receiver != owner
            || owner == address(0) || digest == bytes32(0) || vault == address(0)) revert InvalidOrder();
        if (block.timestamp > deadline) revert Expired();
        if (nonceUsed[owner][nonce]) revert UsedNonce();
        Admission memory a = vaultAdmission[vault];
        if (a.codeHash == bytes32(0) || vault.codehash != a.codeHash
            || IETFPositionVault(vault).definitionHash() != a.definitionHash
            || !IETFPositionFactory(factory).isVault(vault)) revert UnadmittedRoute();
    }

    function _asset(IETFPositionVault v, uint256 i, bool requireAdmitted) private view returns (address asset,
        uint256 unit, bytes32 hash) {
        (asset, unit, hash) = v.assetAt(i);
        if (asset == usdc || asset.codehash != hash
            || (requireAdmitted && IETFPositionFactory(factory).admittedAssetCodeHash(asset) != hash))
            revert UnadmittedRoute();
    }

    function _venue(address vault, address asset, address venue) private view {
        bytes32 hash = venueCodeHash[vault][asset][venue];
        if (venue == address(0) || hash == bytes32(0) || venue.codehash != hash)
            revert UnadmittedRoute();
    }

    function _pull(address token, address owner, uint256 amount) private {
        uint256 beforeHere = IETFPositionToken(token).balanceOf(address(this));
        uint256 beforeOwner = IETFPositionToken(token).balanceOf(owner);
        _call(token, abi.encodeWithSelector(0x23b872dd, owner, address(this), amount));
        if (IETFPositionToken(token).balanceOf(address(this)) != beforeHere + amount
            || IETFPositionToken(token).balanceOf(owner) != beforeOwner - amount)
            revert BalanceMismatch();
    }

    function _send(address token, address receiver, uint256 amount) private {
        uint256 beforeHere = IETFPositionToken(token).balanceOf(address(this));
        uint256 beforeReceiver = IETFPositionToken(token).balanceOf(receiver);
        _call(token, abi.encodeWithSelector(0xa9059cbb, receiver, amount));
        if (IETFPositionToken(token).balanceOf(address(this)) != beforeHere - amount
            || IETFPositionToken(token).balanceOf(receiver) != beforeReceiver + amount)
            revert BalanceMismatch();
    }

    function _approve(address token, address spender, uint256 amount) private {
        _call(token, abi.encodeWithSelector(0x095ea7b3, spender, 0));
        if (amount != 0) _call(token, abi.encodeWithSelector(0x095ea7b3, spender, amount));
    }

    function _call(address token, bytes memory data) private {
        (bool ok, bytes memory response) = token.call(data);
        if (!ok || (response.length != 0 && (response.length != 32 || !abi.decode(response, (bool)))))
            revert TokenCallFailed();
    }
}
