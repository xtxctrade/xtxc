const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { ethers } = require('ethers');

const RPC = 'https://testnet-rpc.monad.xyz';
const FLOW_ABI = [
  'function CHAIN_ID() view returns (uint256)',
  'function factory() view returns (address)',
  'function usdc() view returns (address)',
  'function nonceUsed(address,uint256) view returns (bool)',
  'event ETFInvested(address indexed vault,address indexed owner,uint256 indexed nonce,bytes32 quoteDigest,uint256 shares,uint256 usdcSpent,uint256 fee,uint256 usdcReturned)',
  'event ETFRedeemed(address indexed vault,address indexed owner,uint256 indexed nonce,bytes32 quoteDigest,uint256 shares,bool cashExit,uint256 usdcReceived,uint256 fee)',
];
const VAULT_ABI = [
  'function totalSupply() view returns (uint256)',
  'function issuancePaused() view returns (bool)',
  'function balanceOf(address) view returns (uint256)',
  'function reserveAt(uint256) view returns (uint256,uint256,uint256)',
];
const TOKEN_ABI = ['function balanceOf(address) view returns (uint256)'];

async function main() {
  const file = process.env.XTXC_PR06_EVIDENCE_PATH;
  if (!file || !path.isAbsolute(file)) throw Error('Absolute PR06 evidence path required');
  const evidence = JSON.parse(fs.readFileSync(file, 'utf8'));
  assert.equal(evidence.chainId, 10143);
  assert.equal(evidence.assetClass, 'DEMO_NO_EQUITY_RIGHTS');
  assert.equal(evidence.accepted, true);
  const provider = new ethers.JsonRpcProvider(RPC, undefined, { batchMaxCount: 1 });
  assert.equal((await provider.getNetwork()).chainId, 10143n);
  for (const address of [evidence.etfPositionFlow, evidence.demoSecondVenue,
    evidence.vault, evidence.factory, evidence.usdc, ...evidence.assets]) {
    assert.notEqual(await provider.getCode(address), '0x');
  }
  const flow = new ethers.Contract(evidence.etfPositionFlow, FLOW_ABI, provider);
  const vault = new ethers.Contract(evidence.vault, VAULT_ABI, provider);
  assert.equal(await flow.CHAIN_ID(), 10143n);
  assert.equal((await flow.factory()).toLowerCase(), evidence.factory.toLowerCase());
  assert.equal((await flow.usdc()).toLowerCase(), evidence.usdc.toLowerCase());
  const expected = {
    cashOnlyInvest: 'ETFInvested', partialHoldingsInvest: 'ETFInvested',
    partialHoldingsInvestRetry: 'ETFInvested',
    partialInKindExit: 'ETFRedeemed', transferredHolderCashExit: 'ETFRedeemed',
    transferredHolderCashExitRetry: 'ETFRedeemed',
    remainingCashExit: 'ETFRedeemed',
  };
  let observed = 0;
  for (const row of evidence.transactions) {
    const receipt = await provider.getTransactionReceipt(row.hash);
    assert.ok(receipt, `missing ${row.label}`);
    if (row.status === 'REVERTED') {
      assert.ok(row.label === 'partialHoldingsInvest'
        || row.label === 'transferredHolderCashExit');
      assert.equal(receipt.status, 0, 'gas-limit failure not preserved');
      const expectedGas = row.label === 'partialHoldingsInvest' ? 650_000n : 600_000n;
      assert.equal(receipt.gasUsed, expectedGas, 'unexpected failed attempt');
      continue;
    }
    assert.equal(receipt.status, 1, `reverted ${row.label}`);
    if (expected[row.label]) {
      const event = receipt.logs.filter(log =>
        log.address.toLowerCase() === evidence.etfPositionFlow.toLowerCase())
        .map(log => { try { return flow.interface.parseLog(log); } catch { return null; } })
        .find(log => log?.name === expected[row.label]);
      assert.ok(event, `missing ${expected[row.label]} in ${row.label}`);
      assert.equal(event.args.vault.toLowerCase(), evidence.vault.toLowerCase());
      if (event.name === 'ETFInvested') {
        assert.equal(event.args.owner.toLowerCase(), evidence.buyer.toLowerCase());
        assert.equal(event.args.shares, 10n ** 18n);
        const spent = row.label === 'cashOnlyInvest' ? 15_000_000n : 8_000_000n;
        const debit = row.label === 'cashOnlyInvest' ? 16_000_000n : 9_000_000n;
        assert.equal(event.args.usdcSpent, spent);
        assert.equal(event.args.fee, spent / 20_000n);
        assert.equal(event.args.usdcReturned, debit - spent - event.args.fee);
      }
      if (event.name === 'ETFRedeemed') {
        const cashExit = row.label !== 'partialInKindExit';
        assert.equal(event.args.cashExit, cashExit);
        assert.equal(event.args.shares,
          row.label === 'remainingCashExit' ? 10n ** 18n : 5n * 10n ** 17n);
        assert.equal(event.args.owner.toLowerCase(),
          row.label.startsWith('transferredHolderCashExit')
            ? evidence.deployer.toLowerCase() : evidence.buyer.toLowerCase());
        if (cashExit) {
          assert.ok(event.args.usdcReceived > 0n);
          assert.equal(event.args.fee,
            (event.args.usdcReceived + event.args.fee) / 20_000n);
        } else {
          assert.equal(event.args.usdcReceived, 0n);
          assert.equal(event.args.fee, 0n);
        }
      }
      observed++;
    }
  }
  assert.equal(observed, 5);
  assert.equal(await vault.totalSupply(), 0n);
  assert.equal(await vault.issuancePaused(), true);
  assert.equal(await vault.balanceOf(evidence.buyer), 0n);
  assert.equal(await vault.balanceOf(evidence.deployer), 0n);
  for (let i = 0; i < 2; i++) {
    const [held, owed] = await vault.reserveAt(i);
    assert.equal(held, 0n); assert.equal(owed, 0n);
  }
  for (const asset of [evidence.usdc, ...evidence.assets]) {
    const token = new ethers.Contract(asset, TOKEN_ABI, provider);
    assert.equal(await token.balanceOf(evidence.etfPositionFlow), 0n,
      'position-flow contract retains no demo funds');
  }
  for (const [owner, nonce] of [[evidence.buyer, 6001n], [evidence.buyer, 6002n],
    [evidence.buyer, 6003n], [evidence.deployer, 6004n], [evidence.buyer, 6005n]]) {
    assert.equal(await flow.nonceUsed(owner, nonce), true);
  }
  console.log(JSON.stringify({ result: 'PR06_CHAIN_READ_VERIFIED', chainId: 10143,
    flow: evidence.etfPositionFlow, vault: evidence.vault,
    receipts: evidence.transactions.length, economicEvents: observed,
    finalSupply: '0', finalVaultReserves: ['0', '0'] }));
}

main().catch(error => { console.error(`PR06 verification failed: ${error.shortMessage || error.message}`);
  process.exitCode = 1; });
