// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice TESTNET-ONLY demo assets. They confer no claim on any real company.
contract MetropolisDemoToken {
    string public name;
    string public symbol;
    uint8 public constant decimals = 6;
    uint256 public totalSupply;
    address public immutable issuer;
    bool public immutable faucetEnabled;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    mapping(address => bool) public claimed;

    event Transfer(address indexed from, address indexed to, uint256 amount);
    event Approval(address indexed owner, address indexed spender, uint256 amount);

    constructor(string memory name_, string memory symbol_, bool faucetEnabled_) {
        require(block.chainid == 10143, "testnet only");
        issuer = msg.sender;
        name = name_;
        symbol = symbol_;
        faucetEnabled = faucetEnabled_;
    }

    function mint(address to, uint256 amount) external {
        require(msg.sender == issuer && to != address(0), "issuer");
        _mint(to, amount);
    }

    function claim() external {
        require(faucetEnabled && !claimed[msg.sender], "claim unavailable");
        claimed[msg.sender] = true;
        _mint(msg.sender, 1_000_000_000); // 1,000 demo dollars
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 approved = allowance[from][msg.sender];
        require(approved >= amount, "allowance");
        allowance[from][msg.sender] = approved - amount;
        _transfer(from, to, amount);
        return true;
    }

    function _mint(address to, uint256 amount) private {
        totalSupply += amount;
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }

    function _transfer(address from, address to, uint256 amount) private {
        require(to != address(0) && balanceOf[from] >= amount, "balance");
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }
}

interface IMetropolisDemoToken {
    function balanceOf(address owner) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// @notice TESTNET-ONLY constant-product execution surface for StockMesh PR04.
/// @dev Reserves are deliberately independent for each deployment, allowing
/// multiple venue quotes without pretending that demo liquidity is real stock.
contract MetropolisDemoVenue {
    address public immutable token0;
    address public immutable token1;
    address public immutable provider;
    uint256 public immutable feeBps;
    uint256 public reserve0;
    uint256 public reserve1;
    uint256 private entered;

    event LiquidityAdded(uint256 amount0, uint256 amount1);
    event DemoSwap(address indexed caller, address indexed recipient, address indexed tokenIn,
        uint256 amountIn, uint256 amountOut);

    constructor(address token0_, address token1_, uint256 feeBps_) {
        require(block.chainid == 10143 && token0_ != token1_
            && token0_.code.length != 0 && token1_.code.length != 0
            && feeBps_ <= 100, "invalid demo venue");
        token0 = token0_;
        token1 = token1_;
        provider = msg.sender;
        feeBps = feeBps_;
    }

    modifier nonReentrant() {
        require(entered == 0, "reentry");
        entered = 1;
        _;
        entered = 0;
    }

    function addLiquidity(uint256 amount0, uint256 amount1) external nonReentrant {
        require(msg.sender == provider && amount0 != 0 && amount1 != 0, "provider");
        uint256 before0 = IMetropolisDemoToken(token0).balanceOf(address(this));
        uint256 before1 = IMetropolisDemoToken(token1).balanceOf(address(this));
        require(IMetropolisDemoToken(token0).transferFrom(msg.sender, address(this), amount0), "token0");
        require(IMetropolisDemoToken(token1).transferFrom(msg.sender, address(this), amount1), "token1");
        require(IMetropolisDemoToken(token0).balanceOf(address(this)) == before0 + amount0
            && IMetropolisDemoToken(token1).balanceOf(address(this)) == before1 + amount1,
            "nonexact funding");
        reserve0 += amount0;
        reserve1 += amount1;
        emit LiquidityAdded(amount0, amount1);
    }

    function quoteExactIn(address tokenIn, uint256 amountIn) public view returns (uint256 amountOut) {
        bool forward = tokenIn == token0;
        require(forward || tokenIn == token1, "token");
        uint256 reserveIn = forward ? reserve0 : reserve1;
        uint256 reserveOut = forward ? reserve1 : reserve0;
        require(reserveIn != 0 && reserveOut != 0 && amountIn != 0, "empty");
        uint256 effective = amountIn * (10_000 - feeBps);
        amountOut = effective * reserveOut / (reserveIn * 10_000 + effective);
    }

    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn,
        uint256 minAmountOut, address recipient) external nonReentrant returns (uint256 amountOut) {
        require((tokenIn == token0 && tokenOut == token1)
            || (tokenIn == token1 && tokenOut == token0), "pair");
        require(recipient != address(0), "recipient");
        amountOut = quoteExactIn(tokenIn, amountIn);
        require(amountOut >= minAmountOut && amountOut != 0, "minimum");
        uint256 beforeIn = IMetropolisDemoToken(tokenIn).balanceOf(address(this));
        require(IMetropolisDemoToken(tokenIn).transferFrom(msg.sender, address(this), amountIn), "input");
        require(IMetropolisDemoToken(tokenIn).balanceOf(address(this)) == beforeIn + amountIn,
            "nonexact input");
        require(IMetropolisDemoToken(tokenOut).transfer(recipient, amountOut), "output");
        if (tokenIn == token0) { reserve0 += amountIn; reserve1 -= amountOut; }
        else { reserve1 += amountIn; reserve0 -= amountOut; }
        emit DemoSwap(msg.sender, recipient, tokenIn, amountIn, amountOut);
    }
}
