const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const { ethers } = require('ethers');

const CHAIN_ID = 10143n;
const RPC = 'https://testnet-rpc.monad.xyz';
const root = path.join(__dirname, '..');
const files = ['ETFPositionFlow.sol', 'ETFVaultShare.sol', 'ETFFactory.sol', 'MetropolisDemoAssets.sol'];
const sources = Object.fromEntries(files.map(name => [name, {
  content: fs.readFileSync(path.join(root, 'src', name), 'utf8'),
}]));
const output = JSON.parse(solc.compile(JSON.stringify({ language: 'Solidity', sources,
  settings: { evmVersion: 'shanghai', viaIR: true, optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } } })));
const errors = (output.errors || []).filter(e => e.severity === 'error');
if (errors.length) throw Error(errors.map(e => e.formattedMessage).join('\n'));
const artifact = (file, name) => output.contracts[file][name];
function keyFrom(file) {
  if (!file || !path.isAbsolute(file)) throw Error('Isolated testnet key file required');
  if ((fs.statSync(file).mode & 0o077) !== 0) throw Error('Testnet key permissions too broad');
  const key = fs.readFileSync(file, 'utf8').trim();
  if (!/^0x[0-9a-fA-F]{64}$/.test(key)) throw Error('Malformed demo key');
  return key;
}

async function main() {
  const provider = new ethers.JsonRpcProvider(RPC);
  assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
  if (!process.argv.includes('--broadcast')) {
    console.log(JSON.stringify({ mode: 'READ_ONLY', chainId: Number(CHAIN_ID),
      label: 'ETF position flow demo, no equity rights', contracts: files }));
    return;
  }
  if (process.env.XTXC_PR06_TESTNET_APPROVED !== 'DEMO_POSITION_FLOW_ONLY'
    || process.argv.some(arg => arg.startsWith('--') && arg !== '--broadcast'))
    throw Error('Explicit PR06 demo broadcast marker required');
  const evidencePath = process.env.XTXC_PR06_EVIDENCE_PATH;
  const pr04Path = process.env.XTXC_PR04_MANIFEST_PATH;
  const pr05Path = process.env.XTXC_PR05_EVIDENCE_PATH;
  if (![evidencePath, pr04Path, pr05Path].every(x => x && path.isAbsolute(x))
    || fs.existsSync(evidencePath) || fs.existsSync(`${evidencePath}.journal`))
    throw Error('Fresh absolute evidence path and prior manifests required');
  const pr04 = JSON.parse(fs.readFileSync(pr04Path, 'utf8'));
  const pr05 = JSON.parse(fs.readFileSync(pr05Path, 'utf8'));
  assert.equal(pr04.chainId, Number(CHAIN_ID));
  assert.equal(pr05.chainId, Number(CHAIN_ID));
  assert.equal(pr04.assetClass, 'DEMO_NO_EQUITY_RIGHTS');
  assert.equal(pr05.assetClass, 'DEMO_NO_EQUITY_RIGHTS');
  const deployer = new ethers.Wallet(keyFrom(process.env.XTXC_TESTNET_DEPLOYER_KEY_FILE), provider);
  const buyer = new ethers.Wallet(keyFrom(process.env.XTXC_TESTNET_BUYER_KEY_FILE), provider);
  assert.equal(deployer.address.toLowerCase(), pr04.deployer.toLowerCase());
  assert.equal(buyer.address.toLowerCase(), pr05.creator.toLowerCase());
  if ((await provider.getBalance(deployer.address)) < ethers.parseEther('0.09')
    || (await provider.getBalance(buyer.address)) < ethers.parseEther('0.04'))
    throw Error('Insufficient demo testnet MON; no transaction sent');
  for (const address of [pr04.cash, pr04.stock, pr04.venueA,
    pr05.demoSecondStock, pr05.etfFactory, pr05.etfVault]) {
    if (await provider.getCode(address) === '0x') throw Error('Prior demo contract missing');
  }
  const tokenAbi = artifact('MetropolisDemoAssets.sol', 'MetropolisDemoToken').abi;
  const venueAbi = artifact('MetropolisDemoAssets.sol', 'MetropolisDemoVenue').abi;
  const vaultAbi = artifact('ETFVaultShare.sol', 'ETFVaultShare').abi;
  const factoryAbi = artifact('ETFFactory.sol', 'ETFFactory').abi;
  const usdc = new ethers.Contract(pr04.cash, tokenAbi, deployer);
  const stock = new ethers.Contract(pr04.stock, tokenAbi, deployer);
  const second = new ethers.Contract(pr05.demoSecondStock, tokenAbi, deployer);
  const venueA = new ethers.Contract(pr04.venueA, venueAbi, deployer);
  const factory = new ethers.Contract(pr05.etfFactory, factoryAbi, deployer);
  const vault = new ethers.Contract(pr05.etfVault, vaultAbi, buyer);
  assert.equal(await factory.isVault(pr05.etfVault), true);
  assert.equal(await vault.definitionHash(), pr05.definitionHash);
  assert.equal(await vault.totalSupply(), 0n);
  assert.equal(await vault.issuancePaused(), true);

  const record = { chainId: Number(CHAIN_ID), assetClass: 'DEMO_NO_EQUITY_RIGHTS',
    label: 'XTXC PR06 wallet-owned ETF position flow', deployer: deployer.address,
    buyer: buyer.address, vault: pr05.etfVault, factory: pr05.etfFactory,
    assets: pr05.assets, usdc: pr04.cash, transactions: [] };
  const journalPath = `${evidencePath}.journal`;
  fs.writeFileSync(journalPath, JSON.stringify(record, null, 2), { flag: 'wx', mode: 0o600 });
  function persist() {
    const next = `${journalPath}.next`;
    fs.writeFileSync(next, JSON.stringify(record, null, 2), { mode: 0o600 });
    fs.renameSync(next, journalPath);
  }
  async function sent(label, txPromise) {
    const tx = await txPromise;
    const row = { label, hash: tx.hash, status: 'SUBMITTED' };
    record.transactions.push(row); persist();
    const receipt = await tx.wait();
    row.status = receipt.status === 1 ? 'CONFIRMED' : 'REVERTED';
    row.blockNumber = receipt.blockNumber;
    row.gasUsed = receipt.gasUsed.toString(); persist();
    if (receipt.status !== 1) throw Error(`${label} reverted: ${tx.hash}`);
    return receipt;
  }
  async function deploy(label, file, name, args) {
    const contract = artifact(file, name);
    const instance = await new ethers.ContractFactory(contract.abi,
      `0x${contract.evm.bytecode.object}`, deployer).deploy(...args);
    await sent(label, Promise.resolve(instance.deploymentTransaction()));
    const address = await instance.getAddress();
    record[label] = address; persist();
    return instance;
  }

  // This second market is test-only. It does not stand for issuer access or
  // liquidity in a real TSLA token.
  const venueB = await deploy('demoSecondVenue', 'MetropolisDemoAssets.sol',
    'MetropolisDemoVenue', [pr04.cash, pr05.demoSecondStock, 30n]);
  await sent('mintDemoUsdcForVenue', usdc.mint(deployer.address, 300_000_000n));
  await sent('mintDemoSecondForVenue', second.mint(deployer.address, 20_000_000n));
  await sent('approveUsdcVenue', usdc.approve(await venueB.getAddress(), 200_000_000n));
  await sent('approveSecondVenue', second.approve(await venueB.getAddress(), 10_000_000n));
  await sent('fundSecondVenue', venueB.addLiquidity(200_000_000n, 10_000_000n));

  const flow = await deploy('etfPositionFlow', 'ETFPositionFlow.sol',
    'ETFPositionFlow', [pr04.cash, pr05.etfFactory, deployer.address]);
  const flowAddress = await flow.getAddress();
  await sent('resumeIssuance', factory.resumeIssuance(pr05.etfVault));
  await sent('admitVault', flow.configureVault(pr05.etfVault, true));
  await sent('admitFirstVenue', flow.configureVenue(pr05.etfVault, pr04.stock, pr04.venueA, true));
  await sent('admitSecondVenue', flow.configureVenue(pr05.etfVault,
    pr05.demoSecondStock, await venueB.getAddress(), true));
  await sent('mintDemoBuyerCash', usdc.mint(buyer.address, 40_000_000n));
  await sent('approveBuyerCash', usdc.connect(buyer).approve(flowAddress, 40_000_000n));

  const scale = 10n ** 18n;
  const deadline = BigInt(Math.floor(Date.now() / 1000) + 3600);
  const buyerFlow = flow.connect(buyer);
  const first = { vault: pr05.etfVault, owner: buyer.address, receiver: buyer.address,
    shares: scale, nonce: 6001n, quoteDigest: ethers.id('xtxc-pr06-cash-only'),
    deadline, maxUsdcDebit: 16_000_000n, feeCap: 800n, legs: [
      { fromWallet: 0n, cashIn: 12_000_000n, minBought: 100_000n, venue: pr04.venueA },
      { fromWallet: 0n, cashIn: 3_000_000n, minBought: 100_000n, venue: await venueB.getAddress() },
    ] };
  const beforeCash = await usdc.balanceOf(buyer.address);
  await sent('cashOnlyInvest', buyerFlow.invest(first));
  assert.equal(await vault.balanceOf(buyer.address), scale);
  assert.equal(beforeCash - await usdc.balanceOf(buyer.address), 15_000_750n);
  assert.equal(await flow.nonceUsed(buyer.address, 6001n), true);
  await sent('approveBuyerShares', vault.approve(flowAddress, 2n * scale));

  await sent('partialInKindExit', buyerFlow.redeemInKind(pr05.etfVault, scale / 2n,
    6002n, ethers.id('xtxc-pr06-kind'), deadline));
  assert.equal(await vault.balanceOf(buyer.address), scale / 2n);
  const walletFirst = await stock.balanceOf(buyer.address);
  const walletSecond = await second.balanceOf(buyer.address);
  assert.ok(walletFirst >= 50_000n && walletSecond >= 50_000n);
  await sent('approveBuyerFirstStock', stock.connect(buyer).approve(flowAddress, 50_000n));
  await sent('approveBuyerSecondStock', second.connect(buyer).approve(flowAddress, 50_000n));
  const partial = { ...first, nonce: 6003n, quoteDigest: ethers.id('xtxc-pr06-partial'),
    maxUsdcDebit: 9_000_000n, legs: [
      { fromWallet: 50_000n, cashIn: 6_000_000n, minBought: 50_000n, venue: pr04.venueA },
      { fromWallet: 50_000n, cashIn: 2_000_000n, minBought: 50_000n,
        venue: await venueB.getAddress() },
    ] };
  await sent('partialHoldingsInvest', buyerFlow.invest(partial));
  assert.equal(await vault.balanceOf(buyer.address), 3n * scale / 2n);
  await sent('transferSharesToOtherWallet', vault.transfer(deployer.address, scale / 2n));
  await sent('approveRecipientShares', vault.connect(deployer).approve(flowAddress, scale / 2n));

  const exits = (owner, shares, nonce) => ({ vault: pr05.etfVault, owner,
    receiver: owner, shares, nonce, quoteDigest: ethers.id(`xtxc-pr06-cash-exit-${nonce}`),
    deadline, minUsdcOut: 1n, feeCap: 1_000n, legs: [
      { venue: pr04.venueA, minUsdcOut: 1n },
      { venue: venueB.target, minUsdcOut: 1n },
    ] });
  const recipientBefore = await usdc.balanceOf(deployer.address);
  await sent('transferredHolderCashExit', flow.connect(deployer).redeemToUsdc(
    exits(deployer.address, scale / 2n, 6004n)));
  assert.ok(await usdc.balanceOf(deployer.address) > recipientBefore);
  assert.equal(await vault.balanceOf(deployer.address), 0n);
  await sent('remainingCashExit', buyerFlow.redeemToUsdc(exits(buyer.address, scale, 6005n)));
  assert.equal(await vault.totalSupply(), 0n);
  assert.equal(await vault.balanceOf(buyer.address), 0n);
  for (let i = 0; i < 2; i++) {
    const [held, owed] = await vault.reserveAt(i);
    assert.equal(held, 0n); assert.equal(owed, 0n);
  }
  assert.equal(await usdc.balanceOf(flowAddress), 0n);
  assert.equal(await stock.balanceOf(flowAddress), 0n);
  assert.equal(await second.balanceOf(flowAddress), 0n);
  record.finalSupply = '0'; record.finalVaultReserves = ['0', '0'];
  record.accepted = true;
  fs.writeFileSync(evidencePath, JSON.stringify(record, null, 2), { flag: 'wx', mode: 0o600 });
  console.log(JSON.stringify({ result: 'PR06_TESTNET_ACCEPTED', flow: flowAddress,
    vault: pr05.etfVault, transactions: record.transactions.map(({ label, hash, status }) =>
      ({ label, hash, status })) }, null, 2));
}

main().catch(error => {
  console.error(`PR06 stopped: ${error.shortMessage || error.message}`);
  process.exitCode = 1;
});
