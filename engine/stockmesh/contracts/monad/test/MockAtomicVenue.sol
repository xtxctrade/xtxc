// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IMockToken {
    function transferFrom(address from, address to, uint256 value) external returns (bool);
    function transfer(address to, uint256 value) external returns (bool);
}

contract MockToken {
    string public name;
    string public symbol;
    uint8 public constant decimals = 6;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    constructor(string memory name_, string memory symbol_) { name = name_; symbol = symbol_; }
    function mint(address to, uint256 value) external { balanceOf[to] += value; }
    function transfer(address to, uint256 value) external returns (bool) {
        require(balanceOf[msg.sender] >= value, "balance");
        balanceOf[msg.sender] -= value;
        balanceOf[to] += value;
        return true;
    }
    function approve(address spender, uint256 value) external returns (bool) {
        allowance[msg.sender][spender] = value;
        return true;
    }
    function transferFrom(address from, address to, uint256 value) external returns (bool) {
        require(balanceOf[from] >= value && allowance[from][msg.sender] >= value, "allowance");
        allowance[from][msg.sender] -= value;
        balanceOf[from] -= value;
        balanceOf[to] += value;
        return true;
    }
}

contract MockAtomicVenue {
    bool public partialSpend;
    bool public wrongReceiver;
    bool public fail;
    function setMode(bool partial_, bool wrongReceiver_, bool fail_) external {
        partialSpend = partial_; wrongReceiver = wrongReceiver_; fail = fail_;
    }
    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn, uint256, address receiver)
        external returns (uint256 amountOut)
    {
        require(!fail, "venue failure");
        uint256 spent = partialSpend ? amountIn / 2 : amountIn;
        require(IMockToken(tokenIn).transferFrom(msg.sender, address(this), spent));
        amountOut = spent;
        require(IMockToken(tokenOut).transfer(wrongReceiver ? address(this) : receiver, amountOut));
    }
}
