// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IMockSpotToken {
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
}

contract MockMondayPool {}

contract MockMondayFactory {
    address public pool;
    constructor(address pool_) { pool = pool_; }
    function setPool(address pool_) external { pool = pool_; }
    function getPool(address, address, uint24) external view returns (address) { return pool; }
}

contract MockMondayRouter {
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
    address public immutable factory;
    bool public partialSpend;
    bool public wrongReceiver;
    bool public fail;
    constructor(address factory_) { factory = factory_; }
    function setMode(bool partial_, bool wrongReceiver_, bool fail_) external {
        partialSpend = partial_; wrongReceiver = wrongReceiver_; fail = fail_;
    }
    function exactInputSingle(ExactInputSingleParams calldata p) external payable returns (uint256 amountOut) {
        require(!fail && p.deadline >= block.timestamp && p.sqrtPriceLimitX96 == 0 && p.fee == 3000, "router");
        uint256 spent = partialSpend ? p.amountIn / 2 : p.amountIn;
        require(IMockSpotToken(p.tokenIn).transferFrom(msg.sender, address(this), spent));
        amountOut = spent;
        require(amountOut >= p.amountOutMinimum);
        require(IMockSpotToken(p.tokenOut).transfer(wrongReceiver ? address(this) : p.recipient, amountOut));
    }
}
