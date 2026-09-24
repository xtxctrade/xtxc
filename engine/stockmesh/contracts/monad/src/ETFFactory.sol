// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "./ETFVaultShare.sol";

/// @notice Registry and one-transaction constructor for fixed-unit ETF vaults.
/// Asset admission is a manual policy decision, not a claim that a token gives
/// equity rights or that an issuer permits its use as collateral.
contract ETFFactory {
    error InvalidDefinition();
    error AssetNotAdmitted();
    error DuplicateDefinition();
    error OnlyGovernor();
    error OnlyGuardian();
    error UnknownVault();
    error Reentered();

    address public immutable governor;
    address public immutable issuanceGuardian;
    uint256 public immutable CHAIN_ID;
    mapping(address => bytes32) public admittedAssetCodeHash;
    mapping(address => mapping(uint256 => address)) public vaultByCreatorNonce;
    mapping(bytes32 => address) public vaultByDefinitionHash;
    mapping(address => bool) public isVault;
    uint256 private entered;

    event AssetAdmissionChanged(address indexed asset, bytes32 codeHash);
    event ETFCreated(address indexed creator, address indexed vault, uint256 indexed nonce,
        bytes32 definitionHash, uint64 version);

    constructor(address issuanceGuardian_) {
        if ((block.chainid != 143 && block.chainid != 10143)
            || issuanceGuardian_ == address(0)) revert InvalidDefinition();
        CHAIN_ID = block.chainid;
        governor = msg.sender;
        issuanceGuardian = issuanceGuardian_;
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

    function configureAsset(address asset, bool admitted) external onlyGovernor {
        if (asset == address(0) || (admitted && asset.code.length == 0)) revert InvalidDefinition();
        bytes32 hash = admitted ? asset.codehash : bytes32(0);
        admittedAssetCodeHash[asset] = hash;
        emit AssetAdmissionChanged(asset, hash);
    }

    function definitionDigest(address creator, string memory name, string memory symbol,
        uint64 version, bytes32 metadataDigest, address[] memory assets,
        uint256[] memory unitsPerShare, uint256 shareGranularity)
        public view returns (bytes32)
    {
        return keccak256(abi.encode(CHAIN_ID, address(this), creator, version,
            keccak256(bytes(name)), keccak256(bytes(symbol)), metadataDigest,
            assets, unitsPerShare, shareGranularity));
    }

    function createETF(string calldata name, string calldata symbol, uint64 version,
        uint256 nonce, bytes32 metadataDigest, address[] calldata assets,
        uint256[] calldata unitsPerShare, uint256 shareGranularity,
        bytes32 expectedDefinitionHash) external nonReentrant returns (address vault) {
        if (block.chainid != CHAIN_ID || version == 0 || bytes(name).length == 0
            || bytes(symbol).length == 0 || assets.length < 2 || assets.length > 16
            || assets.length != unitsPerShare.length || expectedDefinitionHash == bytes32(0))
            revert InvalidDefinition();
        if (vaultByCreatorNonce[msg.sender][nonce] != address(0)) revert DuplicateDefinition();
        bytes32 digest = definitionDigest(msg.sender, name, symbol, version,
            metadataDigest, assets, unitsPerShare, shareGranularity);
        if (digest != expectedDefinitionHash) revert InvalidDefinition();
        if (vaultByDefinitionHash[digest] != address(0)) revert DuplicateDefinition();
        for (uint256 i; i < assets.length; ++i) {
            bytes32 hash = admittedAssetCodeHash[assets[i]];
            if (hash == bytes32(0) || assets[i].codehash != hash) revert AssetNotAdmitted();
        }
        bytes32 salt = keccak256(abi.encode(msg.sender, nonce, digest));
        vault = address(new ETFVaultShare{salt: salt}(address(this), msg.sender,
            name, symbol, version, digest, assets, unitsPerShare, shareGranularity));
        vaultByCreatorNonce[msg.sender][nonce] = vault;
        vaultByDefinitionHash[digest] = vault;
        isVault[vault] = true;
        emit ETFCreated(msg.sender, vault, nonce, digest, version);
    }

    /// @dev Guardian may halt new issuance, but cannot resume or change claims.
    /// Existing shares remain redeemable while issuance is paused.
    function pauseIssuance(address vault) external {
        if (msg.sender != issuanceGuardian && msg.sender != governor) revert OnlyGuardian();
        if (!isVault[vault]) revert UnknownVault();
        ETFVaultShare(vault).setIssuancePaused(true);
    }

    function resumeIssuance(address vault) external onlyGovernor {
        if (!isVault[vault]) revert UnknownVault();
        ETFVaultShare(vault).setIssuancePaused(false);
    }
}
