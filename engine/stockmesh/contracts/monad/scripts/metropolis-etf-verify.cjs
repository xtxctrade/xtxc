const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { ethers } = require('ethers');

const RPC = 'https://testnet-rpc.monad.xyz';
const CHAIN_ID = 10143n;
const FACTORY_ABI = [
  'function CHAIN_ID() view returns (uint256)',
  'function vaultByCreatorNonce(address,uint256) view returns (address)',
  'function isVault(address) view returns (bool)',
  'event ETFCreated(address indexed creator,address indexed vault,uint256 indexed nonce,bytes32 definitionHash,uint64 version)',
];
const VAULT_ABI = [
  'function factory() view returns (address)',
  'function creator() view returns (address)',
  'function definitionHash() view returns (bytes32)',
  'function assetCount() view returns (uint256)',
  'function assetAt(uint256) view returns (address,uint256,bytes32)',
  'function totalSupply() view returns (uint256)',
  'function reserveAt(uint256) view returns (uint256,uint256,uint256)',
  'event Issued(address indexed payer,address indexed receiver,uint256 shareAtoms)',
  'event Redeemed(address indexed owner,address indexed receiver,uint256 shareAtoms)',
];

async function main() {
  const file = process.env.XTXC_PR05_EVIDENCE_PATH;
  if (!file || !path.isAbsolute(file)) throw Error('Absolute evidence path required');
  const evidence = JSON.parse(fs.readFileSync(file, 'utf8'));
  assert.equal(evidence.chainId, Number(CHAIN_ID));
  assert.equal(evidence.assetClass, 'DEMO_NO_EQUITY_RIGHTS');
  assert.equal(evidence.accepted, true);
  const provider = new ethers.JsonRpcProvider(RPC);
  assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
  for (const address of [evidence.demoSecondStock, evidence.sourceStock,
    evidence.etfFactory, evidence.etfVault]) {
    assert.notEqual(await provider.getCode(address), '0x');
  }
  const factory = new ethers.Contract(evidence.etfFactory, FACTORY_ABI, provider);
  const vault = new ethers.Contract(evidence.etfVault, VAULT_ABI, provider);
  assert.equal(await factory.CHAIN_ID(), CHAIN_ID);
  assert.equal(await factory.vaultByCreatorNonce(evidence.creator, 1n), evidence.etfVault);
  assert.equal(await factory.isVault(evidence.etfVault), true);
  assert.equal(await vault.factory(), evidence.etfFactory);
  assert.equal(await vault.creator(), evidence.creator);
  assert.equal(await vault.definitionHash(), evidence.definitionHash);
  assert.equal(await vault.assetCount(), 2n);
  assert.equal(await vault.totalSupply(), 0n);
  for (let i = 0; i < 2; i++) {
    const [asset, unit] = await vault.assetAt(i);
    assert.equal(asset.toLowerCase(), evidence.assets[i].toLowerCase());
    assert.equal(unit.toString(), evidence.unitsPerShare[i]);
    const [held, owed, surplus] = await vault.reserveAt(i);
    assert.deepEqual([held, owed, surplus], [0n, 0n, 0n]);
  }
  const expectedEvents = { createETF: 'ETFCreated', mintShare: 'Issued',
    redeemTransferred: 'Redeemed', redeemRemainder: 'Redeemed' };
  const interfaces = { ETFCreated: factory.interface, Issued: vault.interface,
    Redeemed: vault.interface };
  const eventAddress = { ETFCreated: evidence.etfFactory,
    Issued: evidence.etfVault, Redeemed: evidence.etfVault };
  for (const row of evidence.transactions) {
    const receipt = await provider.getTransactionReceipt(row.hash);
    assert.ok(receipt, `missing receipt ${row.label}`);
    assert.equal(receipt.status, 1, `reverted receipt ${row.label}`);
    if (expectedEvents[row.label]) {
      const name = expectedEvents[row.label];
      assert.ok(receipt.logs.some(log => {
        if (log.address.toLowerCase() !== eventAddress[name].toLowerCase()) return false;
        try { return interfaces[name].parseLog(log)?.name === name; } catch { return false; }
      }), `missing ${name} event in ${row.label}`);
    }
  }
  console.log(JSON.stringify({ result: 'PR05_CHAIN_READ_VERIFIED', chainId: Number(CHAIN_ID),
    factory: evidence.etfFactory, vault: evidence.etfVault, receipts: evidence.transactions.length,
    finalSupply: '0', finalVaultReserves: ['0', '0'] }));
}

main().catch(error => { console.error(`PR05 verification failed: ${error.shortMessage || error.message}`); process.exitCode = 1; });
