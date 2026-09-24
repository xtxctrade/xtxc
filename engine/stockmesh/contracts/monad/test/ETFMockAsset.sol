// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IETFReenter {
    function mint(uint256 shares, address receiver) external;
}

contract ETFMockAsset {
    string public name;
    string public symbol;
    uint8 public constant decimals = 6;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    uint256 public feeBps;
    bool public failTransfer;
    bool public failTransferFrom;
    address public reenterVault;

    constructor(string memory name_, string memory symbol_) { name = name_; symbol = symbol_; }
    function mint(address to, uint256 amount) external { balanceOf[to] += amount; }
    function burn(address from, uint256 amount) external { balanceOf[from] -= amount; }
    function configure(uint256 fee_, bool failTransfer_, bool failTransferFrom_, address reenter_) external {
        require(fee_ <= 1000);
        feeBps = fee_;
        failTransfer = failTransfer_;
        failTransferFrom = failTransferFrom_;
        reenterVault = reenter_;
    }
    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }
    function transfer(address to, uint256 amount) external returns (bool) {
        require(!failTransfer && balanceOf[msg.sender] >= amount, "transfer");
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount - amount * feeBps / 10_000;
        return true;
    }
    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        require(!failTransferFrom && balanceOf[from] >= amount
            && allowance[from][msg.sender] >= amount, "transferFrom");
        if (reenterVault != address(0)) IETFReenter(reenterVault).mint(1e12, from);
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount - amount * feeBps / 10_000;
        return true;
    }
}
