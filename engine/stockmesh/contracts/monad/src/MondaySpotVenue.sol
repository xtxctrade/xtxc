// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Narrow Monday Spot V3 adapter for one admitted stock/USDC pool.
/// @dev This is not a generic call proxy. Deploy one instance per product/pool,
/// then separately admit its address in StockMeshExecutor after rights review.
interface IMondaySpotRouter {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function factory() external view returns (address);
    function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256);
}

interface IMondaySpotFactory {
    function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
}

interface IMondaySpotToken {
    function balanceOf(address owner) external view returns (uint256);
    function allowance(address owner, address spender) external view returns (uint256);
}

contract MondaySpotVenue {
    error InvalidConfiguration();
    error Unauthorized();
    error InvalidRoute();
    error TokenCallFailed();
    error BalanceMismatch();
    error OutputBelowMinimum();

    address public immutable executor;
    address public immutable usdc;
    address public immutable stock;
    address public immutable router;
    address public immutable factory;
    address public immutable pool;
    uint24 public immutable poolFee;
    bytes32 public immutable routerCodeHash;
    bytes32 public immutable factoryCodeHash;
    bytes32 public immutable poolCodeHash;

    constructor(address executor_, address usdc_, address stock_, address router_, address pool_, uint24 poolFee_) {
        if (block.chainid != 143 || executor_ == address(0) || usdc_ == address(0)
            || stock_ == address(0) || router_ == address(0) || pool_ == address(0)
            || usdc_ == stock_ || poolFee_ == 0 || executor_.code.length == 0
            || usdc_.code.length == 0 || stock_.code.length == 0
            || router_.code.length == 0 || pool_.code.length == 0) revert InvalidConfiguration();
        address factory_ = IMondaySpotRouter(router_).factory();
        if (factory_.code.length == 0 || IMondaySpotFactory(factory_).getPool(usdc_, stock_, poolFee_) != pool_)
            revert InvalidConfiguration();
        executor = executor_;
        usdc = usdc_;
        stock = stock_;
        router = router_;
        factory = factory_;
        pool = pool_;
        poolFee = poolFee_;
        routerCodeHash = router_.codehash;
        factoryCodeHash = factory_.codehash;
        poolCodeHash = pool_.codehash;
    }

    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn, uint256 minAmountOut, address recipient)
        external returns (uint256 amountOut)
    {
        if (msg.sender != executor) revert Unauthorized();
        if (recipient != executor || amountIn == 0 || minAmountOut == 0
            || !((tokenIn == usdc && tokenOut == stock) || (tokenIn == stock && tokenOut == usdc))
            || router.codehash != routerCodeHash || factory.codehash != factoryCodeHash
            || pool.codehash != poolCodeHash || IMondaySpotRouter(router).factory() != factory
            || IMondaySpotFactory(factory).getPool(tokenIn, tokenOut, poolFee) != pool)
            revert InvalidRoute();

        uint256 inputBefore = IMondaySpotToken(tokenIn).balanceOf(address(this));
        uint256 outputBefore = IMondaySpotToken(tokenOut).balanceOf(executor);
        _tokenCall(tokenIn, abi.encodeWithSelector(0x23b872dd, executor, address(this), amountIn));
        if (IMondaySpotToken(tokenIn).balanceOf(address(this)) != inputBefore + amountIn)
            revert BalanceMismatch();
        _tokenCall(tokenIn, abi.encodeWithSelector(0x095ea7b3, router, 0));
        _tokenCall(tokenIn, abi.encodeWithSelector(0x095ea7b3, router, amountIn));
        amountOut = IMondaySpotRouter(router).exactInputSingle(
            IMondaySpotRouter.ExactInputSingleParams({
                tokenIn: tokenIn, tokenOut: tokenOut, fee: poolFee, recipient: executor,
                deadline: block.timestamp, amountIn: amountIn,
                amountOutMinimum: minAmountOut, sqrtPriceLimitX96: 0
            })
        );
        _tokenCall(tokenIn, abi.encodeWithSelector(0x095ea7b3, router, 0));
        if (IMondaySpotToken(tokenIn).allowance(address(this), router) != 0
            || IMondaySpotToken(tokenIn).balanceOf(address(this)) != inputBefore
            || IMondaySpotToken(tokenOut).balanceOf(executor) != outputBefore + amountOut)
            revert BalanceMismatch();
        if (amountOut < minAmountOut) revert OutputBelowMinimum();
    }

    function _tokenCall(address token, bytes memory data) private {
        (bool ok, bytes memory response) = token.call(data);
        if (!ok || (response.length != 0 && (response.length != 32 || !abi.decode(response, (bool)))))
            revert TokenCallFailed();
    }
}
