// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IETFAsset {
    function balanceOf(address owner) external view returns (uint256);
}

/// @notice Fixed-unit, in-kind basket shares. No oracle, strategy manager,
/// upgrade hook, discretionary withdrawal, or standalone share burn exists.
/// The factory reviews constituents before creation; this vault still checks
/// exact token balance deltas for every issue and redemption.
contract ETFVaultShare {
    error InvalidDefinition();
    error InvalidShares();
    error IssuancePaused();
    error AssetCodeChanged();
    error BackingDeficit();
    error BalanceMismatch();
    error TokenCallFailed();
    error Reentered();
    error OnlyFactory();

    uint256 public constant SHARE_SCALE = 1e18;
    uint256 public constant MAX_UNIT_ATOMS = 1e20;
    uint8 public constant decimals = 18;
    string public name;
    string public symbol;
    address public immutable factory;
    address public immutable creator;
    uint64 public immutable version;
    bytes32 public immutable definitionHash;
    uint256 public immutable shareGranularity;
    uint256 public totalSupply;
    bool public issuancePaused;

    address[] private assets;
    uint256[] private unitsPerShare;
    bytes32[] private assetCodeHashes;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    uint256 private entered;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    event Issued(address indexed payer, address indexed receiver, uint256 shareAtoms);
    event Redeemed(address indexed owner, address indexed receiver, uint256 shareAtoms);
    event IssuancePausedChanged(bool paused);

    constructor(
        address factory_, address creator_, string memory name_, string memory symbol_,
        uint64 version_, bytes32 definitionHash_, address[] memory assets_,
        uint256[] memory unitsPerShare_, uint256 shareGranularity_
    ) {
        if (msg.sender != factory_ || factory_ == address(0) || creator_ == address(0)
            || definitionHash_ == bytes32(0) || bytes(name_).length == 0
            || bytes(symbol_).length == 0 || version_ == 0
            || assets_.length < 2 || assets_.length > 16
            || assets_.length != unitsPerShare_.length
            || shareGranularity_ == 0 || shareGranularity_ > SHARE_SCALE
            || SHARE_SCALE % shareGranularity_ != 0) revert InvalidDefinition();
        factory = factory_;
        creator = creator_;
        name = name_;
        symbol = symbol_;
        version = version_;
        definitionHash = definitionHash_;
        shareGranularity = shareGranularity_;
        for (uint256 i; i < assets_.length; ++i) {
            address asset = assets_[i];
            uint256 unit = unitsPerShare_[i];
            if (asset == address(0) || asset == address(this) || asset.code.length == 0
                || unit == 0 || unit > MAX_UNIT_ATOMS
                || unit * shareGranularity_ % SHARE_SCALE != 0) revert InvalidDefinition();
            for (uint256 j; j < i; ++j) {
                if (assets_[j] == asset) revert InvalidDefinition();
            }
            assets.push(asset);
            unitsPerShare.push(unit);
            assetCodeHashes.push(asset.codehash);
        }
    }

    modifier nonReentrant() {
        if (entered != 0) revert Reentered();
        entered = 1;
        _;
        entered = 0;
    }

    function assetCount() external view returns (uint256) { return assets.length; }

    function assetAt(uint256 index) external view returns (address, uint256, bytes32) {
        return (assets[index], unitsPerShare[index], assetCodeHashes[index]);
    }

    function previewClaim(uint256 shareAtoms) public view returns (uint256[] memory amounts) {
        _validShares(shareAtoms);
        amounts = new uint256[](assets.length);
        for (uint256 i; i < assets.length; ++i) {
            amounts[i] = shareAtoms * unitsPerShare[i] / SHARE_SCALE;
        }
    }

    /// @return held Actual vault balance, including any unsolicited donation.
    /// @return owed Amount backing all outstanding shares.
    /// @return surplus Non-claimable donation; it never creates shares.
    function reserveAt(uint256 index) external view returns (uint256 held, uint256 owed, uint256 surplus) {
        held = IETFAsset(assets[index]).balanceOf(address(this));
        owed = totalSupply * unitsPerShare[index] / SHARE_SCALE;
        surplus = held > owed ? held - owed : 0;
    }

    function setIssuancePaused(bool paused) external {
        if (msg.sender != factory) revert OnlyFactory();
        issuancePaused = paused;
        emit IssuancePausedChanged(paused);
    }

    function mint(uint256 shareAtoms, address receiver) external nonReentrant {
        if (issuancePaused) revert IssuancePaused();
        _validShares(shareAtoms);
        if (receiver == address(0) || receiver == address(this)) revert InvalidShares();
        uint256 newSupply = totalSupply + shareAtoms;
        uint256[] memory amounts = previewClaim(shareAtoms);
        for (uint256 i; i < assets.length; ++i) {
            address asset = assets[i];
            if (asset.codehash != assetCodeHashes[i]) revert AssetCodeChanged();
            uint256 vaultBefore = IETFAsset(asset).balanceOf(address(this));
            uint256 payerBefore = IETFAsset(asset).balanceOf(msg.sender);
            if (vaultBefore < totalSupply * unitsPerShare[i] / SHARE_SCALE) revert BackingDeficit();
            _safeCall(asset, abi.encodeWithSelector(0x23b872dd, msg.sender, address(this), amounts[i]));
            if (IETFAsset(asset).balanceOf(address(this)) != vaultBefore + amounts[i]
                || IETFAsset(asset).balanceOf(msg.sender) != payerBefore - amounts[i]
                || IETFAsset(asset).balanceOf(address(this)) < newSupply * unitsPerShare[i] / SHARE_SCALE)
                revert BalanceMismatch();
        }
        totalSupply = newSupply;
        balanceOf[receiver] += shareAtoms;
        emit Transfer(address(0), receiver, shareAtoms);
        emit Issued(msg.sender, receiver, shareAtoms);
    }

    function redeem(uint256 shareAtoms, address receiver) external nonReentrant {
        _validShares(shareAtoms);
        if (receiver == address(0) || receiver == address(this)
            || balanceOf[msg.sender] < shareAtoms) revert InvalidShares();
        uint256[] memory amounts = previewClaim(shareAtoms);
        uint256 newSupply = totalSupply - shareAtoms;
        for (uint256 i; i < assets.length; ++i) {
            if (IETFAsset(assets[i]).balanceOf(address(this))
                < totalSupply * unitsPerShare[i] / SHARE_SCALE) revert BackingDeficit();
        }
        balanceOf[msg.sender] -= shareAtoms;
        totalSupply = newSupply;
        emit Transfer(msg.sender, address(0), shareAtoms);
        for (uint256 i; i < assets.length; ++i) {
            address asset = assets[i];
            uint256 vaultBefore = IETFAsset(asset).balanceOf(address(this));
            uint256 receiverBefore = IETFAsset(asset).balanceOf(receiver);
            _safeCall(asset, abi.encodeWithSelector(0xa9059cbb, receiver, amounts[i]));
            if (IETFAsset(asset).balanceOf(address(this)) != vaultBefore - amounts[i]
                || IETFAsset(asset).balanceOf(receiver) != receiverBefore + amounts[i]
                || IETFAsset(asset).balanceOf(address(this)) < newSupply * unitsPerShare[i] / SHARE_SCALE)
                revert BalanceMismatch();
        }
        emit Redeemed(msg.sender, receiver, shareAtoms);
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        if (spender == address(0)) revert InvalidShares();
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _move(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 approved = allowance[from][msg.sender];
        if (approved != type(uint256).max) {
            if (approved < amount) revert InvalidShares();
            allowance[from][msg.sender] = approved - amount;
            emit Approval(from, msg.sender, approved - amount);
        }
        _move(from, to, amount);
        return true;
    }

    function _move(address from, address to, uint256 amount) private {
        if (from == address(0) || to == address(0) || to == address(this)
            || amount % shareGranularity != 0 || balanceOf[from] < amount) revert InvalidShares();
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }

    function _validShares(uint256 shareAtoms) private view {
        if (shareAtoms == 0 || shareAtoms % shareGranularity != 0) revert InvalidShares();
    }

    function _safeCall(address token, bytes memory data) private {
        (bool ok, bytes memory response) = token.call(data);
        if (!ok || (response.length != 0 && (response.length != 32 || !abi.decode(response, (bool)))))
            revert TokenCallFailed();
    }
}
