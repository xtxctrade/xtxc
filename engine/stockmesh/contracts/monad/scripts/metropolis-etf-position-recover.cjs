// Resume the PR06 *testnet-only* demo after a journaled RPC interruption.
// Never replay a submitted transaction whose receipt is still unknown.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const { ethers } = require('ethers');

const CHAIN_ID = 10143n;
const RPC = 'https://testnet-rpc.monad.xyz';
const SCALE = 10n ** 18n;
const PRIOR = [
  'demoSecondVenue', 'mintDemoUsdcForVenue', 'mintDemoSecondForVenue',
  'approveUsdcVenue', 'approveSecondVenue', 'fundSecondVenue',
  'etfPositionFlow', 'resumeIssuance', 'admitVault', 'admitFirstVenue',
  'admitSecondVenue', 'mintDemoBuyerCash', 'approveBuyerCash',
  'cashOnlyInvest', 'approveBuyerShares', 'partialInKindExit',
  'approveBuyerFirstStock', 'approveBuyerSecondStock',
];
const REMAINING = [
  'fundBuyerGas', 'partialHoldingsInvest', 'partialHoldingsInvestRetry',
  'transferSharesToOtherWallet',
  'approveRecipientShares', 'transferredHolderCashExit',
  'transferredHolderCashExitRetry',
  'remainingCashExit', 'pauseIssuanceAfterDemo',
];
let phase = 'preflight';

function readKey(file) {
  if (!file || !path.isAbsolute(file)) throw Error('Isolated testnet key file required');
  if ((fs.statSync(file).mode & 0o077) !== 0) throw Error('Testnet key permissions too broad');
  const key = fs.readFileSync(file, 'utf8').trim();
  if (!/^0x[0-9a-fA-F]{64}$/.test(key)) throw Error('Malformed demo key');
  return key;
}

function absoluteFile(variable) {
  const file = process.env[variable];
  if (!file || !path.isAbsolute(file)) throw Error(`${variable} must be an absolute path`);
  return file;
}

function flowAbi() {
  const file = path.join(__dirname, '..', 'src', 'ETFPositionFlow.sol');
  const output = JSON.parse(solc.compile(JSON.stringify({
    language: 'Solidity', sources: { 'ETFPositionFlow.sol': {
      content: fs.readFileSync(file, 'utf8'),
    } }, settings: { outputSelection: { '*': { '*': ['abi'] } } },
  })));
  const errors = (output.errors || []).filter(e => e.severity === 'error');
  if (errors.length) throw Error(errors.map(e => e.formattedMessage).join('\n'));
  return output.contracts['ETFPositionFlow.sol'].ETFPositionFlow.abi;
}

async function main() {
  if (process.env.XTXC_PR06_TESTNET_APPROVED !== 'DEMO_POSITION_FLOW_ONLY'
    || process.argv.length !== 3 || process.argv[2] !== '--resume') {
    throw Error('Explicit testnet-only PR06 recovery marker required');
  }
  const evidencePath = absoluteFile('XTXC_PR06_EVIDENCE_PATH');
  const journalPath = `${evidencePath}.journal`;
  if (fs.existsSync(evidencePath) || !fs.existsSync(journalPath))
    throw Error('Existing unfinished PR06 journal required');
  const record = JSON.parse(fs.readFileSync(journalPath, 'utf8'));
  const pr04 = JSON.parse(fs.readFileSync(absoluteFile('XTXC_PR04_MANIFEST_PATH'), 'utf8'));
  const pr05 = JSON.parse(fs.readFileSync(absoluteFile('XTXC_PR05_EVIDENCE_PATH'), 'utf8'));
  for (const item of [record, pr04, pr05]) {
    assert.equal(item.chainId, Number(CHAIN_ID));
    assert.equal(item.assetClass, 'DEMO_NO_EQUITY_RIGHTS');
  }
  assert.equal(record.vault.toLowerCase(), pr05.etfVault.toLowerCase());
  assert.equal(record.factory.toLowerCase(), pr05.etfFactory.toLowerCase());
  assert.equal(record.usdc.toLowerCase(), pr04.cash.toLowerCase());
  assert.deepEqual(record.assets.map(x => x.toLowerCase()),
    pr05.assets.map(x => x.toLowerCase()));
  const labels = record.transactions.map(x => x.label);
  assert.deepEqual(labels.slice(0, PRIOR.length), PRIOR);
  assert.deepEqual(labels.slice(PRIOR.length), REMAINING.slice(0, labels.length - PRIOR.length));

  phase = 'network';
  const provider = new ethers.JsonRpcProvider(RPC, undefined, { batchMaxCount: 1 });
  assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
  const deployer = new ethers.Wallet(readKey(absoluteFile('XTXC_TESTNET_DEPLOYER_KEY_FILE')), provider);
  const buyer = new ethers.Wallet(readKey(absoluteFile('XTXC_TESTNET_BUYER_KEY_FILE')), provider);
  assert.equal(deployer.address.toLowerCase(), record.deployer.toLowerCase());
  assert.equal(buyer.address.toLowerCase(), record.buyer.toLowerCase());
  const tokenAbi = [
    'function balanceOf(address) view returns (uint256)',
    'function allowance(address,address) view returns (uint256)',
    'function approve(address,uint256) returns (bool)',
  ];
  const vaultAbi = [
    'function balanceOf(address) view returns (uint256)',
    'function totalSupply() view returns (uint256)',
    'function issuancePaused() view returns (bool)',
    'function reserveAt(uint256) view returns (uint256,uint256,uint256)',
    'function transfer(address,uint256) returns (bool)',
    'function approve(address,uint256) returns (bool)',
  ];
  const factoryAbi = ['function pauseIssuance(address)'];
  const flow = new ethers.Contract(record.etfPositionFlow, flowAbi(), buyer);
  const vault = new ethers.Contract(record.vault, vaultAbi, buyer);
  const stock = new ethers.Contract(pr04.stock, tokenAbi, buyer);
  const second = new ethers.Contract(pr05.demoSecondStock, tokenAbi, buyer);
  const usdc = new ethers.Contract(record.usdc, tokenAbi, buyer);
  const factory = new ethers.Contract(record.factory, factoryAbi, deployer);
  phase = 'deployed-code';
  for (const address of [record.etfPositionFlow, record.demoSecondVenue,
    record.vault, record.factory, record.usdc, ...record.assets]) {
    if (await provider.getCode(address) === '0x') throw Error('Prior demo contract missing');
  }
  phase = 'vault-state';
  if (await vault.issuancePaused()) throw Error('Vault unexpectedly paused before recovery');

  function persist() {
    const next = `${journalPath}.next`;
    fs.writeFileSync(next, JSON.stringify(record, null, 2), { mode: 0o600 });
    fs.renameSync(next, journalPath);
  }
  async function confirmed(row) {
    let receipt;
    for (let attempt = 0; attempt < 40; attempt++) {
      const response = await fetch(RPC, { method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ jsonrpc: '2.0', id: 1,
          method: 'eth_getTransactionReceipt', params: [row.hash] }),
      });
      const payload = await response.json();
      if (!response.ok || payload.error)
        throw Error(`${row.label} receipt RPC error; do not resubmit`);
      receipt = payload.result;
      if (receipt) break;
      await new Promise(resolve => setTimeout(resolve, 1000));
    }
    if (!receipt) throw Error(`${row.label} receipt UNKNOWN; do not resubmit`);
    const failedGas = row.label === 'partialHoldingsInvest' ? 650_000n
      : row.label === 'transferredHolderCashExit' ? 600_000n : null;
    const expectedFailure = failedGas !== null;
    if (expectedFailure) {
      if (receipt.status !== '0x0' || BigInt(receipt.gasUsed) !== failedGas)
        throw Error('The recorded gas-limit failure has different onchain results');
    } else if (receipt.status !== '0x1') {
      throw Error(`${row.label} reverted: ${row.hash}`);
    }
    const status = expectedFailure ? 'REVERTED' : 'CONFIRMED';
    if (row.status !== status) {
      row.status = status;
      row.blockNumber = Number(BigInt(receipt.blockNumber));
      row.gasUsed = BigInt(receipt.gasUsed).toString();
      persist();
    }
    return receipt;
  }
  for (const row of record.transactions) {
    phase = `receipt:${row.label}`;
    await confirmed(row);
  }
  phase = 'nonce-state';
  if (await flow.nonceUsed(buyer.address, 6003n)
    && !labels.includes('partialHoldingsInvestRetry'))
    throw Error('Partial invest executed without a journal row; stop');
  if (await flow.nonceUsed(deployer.address, 6004n)
    && !labels.includes('transferredHolderCashExitRetry'))
    throw Error('Recipient exit executed without a journal row; stop');
  if (await flow.nonceUsed(buyer.address, 6005n)
    && !labels.includes('remainingCashExit'))
    throw Error('Buyer exit executed without a journal row; stop');

  async function sent(label, txPromise) {
    phase = `transaction:${label}`;
    const existing = record.transactions.find(x => x.label === label);
    if (existing) return confirmed(existing);
    const tx = await txPromise();
    const row = { label, hash: tx.hash, status: 'SUBMITTED' };
    record.transactions.push(row);
    persist();
    return confirmed(row);
  }
  if (!record.transactions.some(x => x.label === 'fundBuyerGas')) {
    if ((await provider.getBalance(buyer.address)) > ethers.parseEther('0.25'))
      throw Error('Buyer gas balance changed; inspect before recovery funding');
  }
  await sent('fundBuyerGas', () => deployer.sendTransaction({
    to: buyer.address, value: ethers.parseEther('0.3'), gasLimit: 21_000n,
  }));
  if ((await provider.getBalance(buyer.address)) < ethers.parseEther('0.15'))
    throw Error('Insufficient buyer testnet MON after funding');

  const deadline = BigInt(Math.floor(Date.now() / 1000) + 3600);
  const partial = {
    vault: record.vault, owner: buyer.address, receiver: buyer.address,
    shares: SCALE, nonce: 6003n, quoteDigest: ethers.id('xtxc-pr06-partial'),
    deadline, maxUsdcDebit: 9_000_000n, feeCap: 800n,
    legs: [
      { fromWallet: 50_000n, cashIn: 6_000_000n, minBought: 50_000n, venue: pr04.venueA },
      { fromWallet: 50_000n, cashIn: 2_000_000n, minBought: 50_000n,
        venue: record.demoSecondVenue },
    ],
  };
  if (!record.transactions.some(x => x.label === 'partialHoldingsInvestRetry')) {
    assert.equal(await flow.nonceUsed(buyer.address, 6003n), false);
    assert.ok(await stock.balanceOf(buyer.address) >= 50_000n);
    assert.ok(await second.balanceOf(buyer.address) >= 50_000n);
    assert.ok(await stock.allowance(buyer.address, record.etfPositionFlow) >= 50_000n);
    assert.ok(await second.allowance(buyer.address, record.etfPositionFlow) >= 50_000n);
    await flow.invest.staticCall(partial, { from: buyer.address });
  }
  await sent('partialHoldingsInvestRetry', () => flow.invest(partial, { gasLimit: 1_100_000n }));
  if (!record.transactions.some(x => x.label === 'transferSharesToOtherWallet')) {
    assert.equal(await vault.balanceOf(buyer.address), 3n * SCALE / 2n);
    assert.equal(await vault.balanceOf(deployer.address), 0n);
  }
  await sent('transferSharesToOtherWallet', () => vault.transfer(
    deployer.address, SCALE / 2n, { gasLimit: 100_000n }));
  await sent('approveRecipientShares', () => vault.connect(deployer).approve(
    record.etfPositionFlow, SCALE / 2n, { gasLimit: 100_000n }));

  function exit(owner, nonce) {
    return { vault: record.vault, owner, receiver: owner,
      shares: owner.toLowerCase() === deployer.address.toLowerCase() ? SCALE / 2n : SCALE,
      nonce, quoteDigest: ethers.id(`xtxc-pr06-cash-exit-${nonce}`), deadline,
      minUsdcOut: 1n, feeCap: 1_000n,
      legs: [{ venue: pr04.venueA, minUsdcOut: 1n },
        { venue: record.demoSecondVenue, minUsdcOut: 1n }],
    };
  }
  await sent('transferredHolderCashExitRetry', () => flow.connect(deployer).redeemToUsdc(
    exit(deployer.address, 6004n), { gasLimit: 900_000n }));
  await sent('remainingCashExit', () => flow.redeemToUsdc(
    exit(buyer.address, 6005n), { gasLimit: 900_000n }));
  await sent('pauseIssuanceAfterDemo', () => factory.pauseIssuance(
    record.vault, { gasLimit: 200_000n }));

  assert.equal(await vault.totalSupply(), 0n);
  assert.equal(await vault.issuancePaused(), true);
  assert.equal(await vault.balanceOf(buyer.address), 0n);
  assert.equal(await vault.balanceOf(deployer.address), 0n);
  for (let i = 0; i < 2; i++) {
    const [held, owed] = await vault.reserveAt(i);
    assert.equal(held, 0n); assert.equal(owed, 0n);
  }
  for (const asset of [record.usdc, ...record.assets]) {
    const token = new ethers.Contract(asset, tokenAbi, provider);
    assert.equal(await token.balanceOf(record.etfPositionFlow), 0n);
  }
  record.finalSupply = '0';
  record.finalVaultReserves = ['0', '0'];
  record.accepted = true;
  fs.writeFileSync(evidencePath, JSON.stringify(record, null, 2), { flag: 'wx', mode: 0o600 });
  console.log(JSON.stringify({ result: 'PR06_TESTNET_ACCEPTED', flow: record.etfPositionFlow,
    vault: record.vault, receipts: record.transactions.length }));
}

main().catch(error => {
  console.error(`PR06 recovery stopped at ${phase}: ${error.shortMessage || error.message}`);
  process.exitCode = 1;
});
