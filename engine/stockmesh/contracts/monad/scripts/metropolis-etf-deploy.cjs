const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const { ethers } = require('ethers');

const CHAIN_ID = 10143n;
const RPC = 'https://testnet-rpc.monad.xyz';
const root = path.join(__dirname, '..');
const files = ['ETFVaultShare.sol', 'ETFFactory.sol', 'MetropolisDemoAssets.sol'];
const sources = Object.fromEntries(files.map(name => [name, {
  content: fs.readFileSync(path.join(root, 'src', name), 'utf8'),
}]));
const output = JSON.parse(solc.compile(JSON.stringify({ language: 'Solidity', sources,
  settings: { evmVersion: 'shanghai', viaIR: true, optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } } })));
const errors = (output.errors || []).filter(e => e.severity === 'error');
if (errors.length) throw new Error(errors.map(e => e.formattedMessage).join('\n'));
const artifact = (file, name) => output.contracts[file][name];
function readTestKey(file) {
  if (!file || !path.isAbsolute(file)) throw Error('Private testnet key file required');
  const stat = fs.statSync(file);
  if ((stat.mode & 0o077) !== 0) throw Error('Testnet key permissions too broad');
  const key = fs.readFileSync(file, 'utf8').trim();
  if (!/^0x[0-9a-fA-F]{64}$/.test(key)) throw Error('Malformed testnet key');
  return key;
}

async function main() {
  const provider = new ethers.JsonRpcProvider(RPC);
  assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
  if (!process.argv.includes('--broadcast')) {
    console.log(JSON.stringify({ mode: 'READ_ONLY', chainId: Number(CHAIN_ID),
      contracts: files, label: 'demo assets, no equity rights' }));
    return;
  }
  if (process.env.XTXC_PR05_TESTNET_APPROVED !== 'DEMO_ETF_ONLY'
    || process.argv.some(arg => arg.startsWith('--') && arg !== '--broadcast'))
    throw Error('Explicit demo-only broadcast marker required');
  const evidencePath = process.env.XTXC_PR05_EVIDENCE_PATH;
  const pr04Path = process.env.XTXC_PR04_MANIFEST_PATH;
  if (!evidencePath || !pr04Path || !path.isAbsolute(evidencePath)
    || !path.isAbsolute(pr04Path) || fs.existsSync(evidencePath))
    throw Error('New absolute evidence path and PR04 manifest required');
  const pr04 = JSON.parse(fs.readFileSync(pr04Path, 'utf8'));
  assert.equal(pr04.chainId, Number(CHAIN_ID));
  assert.equal(pr04.assetClass, 'DEMO_NO_EQUITY_RIGHTS');
  assert.equal((await provider.getCode(pr04.stock)) !== '0x', true);
  const deployer = new ethers.Wallet(readTestKey(process.env.XTXC_TESTNET_DEPLOYER_KEY_FILE), provider);
  const buyer = new ethers.Wallet(readTestKey(process.env.XTXC_TESTNET_BUYER_KEY_FILE), provider);
  assert.equal(deployer.address.toLowerCase(), pr04.deployer.toLowerCase());
  if ((await provider.getBalance(deployer.address)) < ethers.parseEther('0.1')
    || (await provider.getBalance(buyer.address)) < ethers.parseEther('0.03'))
    throw Error('Insufficient demo testnet gas; no transaction sent');

  const record = { chainId: Number(CHAIN_ID), assetClass: 'DEMO_NO_EQUITY_RIGHTS',
    label: 'XTXC PR05 fixed-unit ETF demo', deployer: deployer.address,
    creator: buyer.address, sourceStock: pr04.stock, transactions: [] };
  const journalPath = `${evidencePath}.journal`;
  if (fs.existsSync(journalPath)) throw Error('Existing journal requires manual recovery');
  fs.writeFileSync(journalPath, JSON.stringify(record, null, 2), { flag: 'wx', mode: 0o600 });
  function persist() {
    const temp = `${journalPath}.next`;
    fs.writeFileSync(temp, JSON.stringify(record, null, 2), { mode: 0o600 });
    fs.renameSync(temp, journalPath);
  }
  async function sent(label, txPromise) {
    const tx = await txPromise;
    const row = { label, hash: tx.hash, status: 'SUBMITTED' };
    record.transactions.push(row);
    persist();
    const receipt = await tx.wait();
    row.status = receipt.status === 1 ? 'CONFIRMED' : 'REVERTED';
    row.blockNumber = receipt.blockNumber;
    row.gasUsed = receipt.gasUsed.toString();
    persist();
    if (receipt.status !== 1) throw Error(`${label} reverted: ${tx.hash}`);
    return receipt;
  }
  async function deploy(label, file, contract, signer, args) {
    const item = artifact(file, contract);
    const instance = await new ethers.ContractFactory(item.abi,
      `0x${item.evm.bytecode.object}`, signer).deploy(...args);
    await sent(label, Promise.resolve(instance.deploymentTransaction()));
    const address = await instance.getAddress();
    record[label] = address;
    persist();
    return instance;
  }

  const second = await deploy('demoSecondStock', 'MetropolisDemoAssets.sol',
    'MetropolisDemoToken', deployer,
    ['Demo TSLA, no equity claim', 'dTSLA', false]);
  const factory = await deploy('etfFactory', 'ETFFactory.sol', 'ETFFactory',
    deployer, [deployer.address]);
  const secondAddress = await second.getAddress();
  const factoryAddress = await factory.getAddress();
  const first = new ethers.Contract(pr04.stock,
    artifact('MetropolisDemoAssets.sol', 'MetropolisDemoToken').abi, buyer);
  if ((await first.balanceOf(buyer.address)) < 100_000n)
    throw Error('Buyer lacks PR04 demo stock; retain journal and stop');
  await sent('mintSecondDemoStock', second.mint(buyer.address, 1_000_000n));
  await sent('admitFirst', factory.configureAsset(pr04.stock, true));
  await sent('admitSecond', factory.configureAsset(secondAddress, true));

  const assets = [pr04.stock, secondAddress];
  const units = [100_000n, 100_000n]; // constituent atoms per whole share
  const granularity = 10n ** 13n;
  const metadata = ethers.id('xtxc-pr05-testnet-no-equity-rights');
  const name = 'XTXC Demo Tech ETF';
  const symbol = 'xdTECH';
  const digest = await factory.definitionDigest(buyer.address, name, symbol,
    1n, metadata, assets, units, granularity);
  const created = await sent('createETF', factory.connect(buyer).createETF(
    name, symbol, 1n, 1n, metadata, assets, units, granularity, digest));
  const parsed = created.logs.map(log => {
    try { return factory.interface.parseLog(log); } catch { return null; }
  }).find(event => event?.name === 'ETFCreated');
  if (!parsed) throw Error('No ETFCreated event');
  const vaultAddress = parsed.args.vault;
  const vaultAbi = artifact('ETFVaultShare.sol', 'ETFVaultShare').abi;
  const vault = new ethers.Contract(vaultAddress, vaultAbi, buyer);
  assert.equal(await factory.vaultByCreatorNonce(buyer.address, 1n), vaultAddress);
  assert.equal(await vault.definitionHash(), digest);
  record.etfVault = vaultAddress;
  record.definitionHash = digest;
  record.assets = assets;
  record.unitsPerShare = units.map(String);
  record.shareGranularity = String(granularity);
  persist();

  const firstBefore = await first.balanceOf(buyer.address);
  const secondBefore = await second.balanceOf(buyer.address);
  await sent('approveFirst', first.approve(vaultAddress, units[0]));
  await sent('approveSecond', second.connect(buyer).approve(vaultAddress, units[1]));
  const scale = 10n ** 18n;
  await sent('mintShare', vault.mint(scale, buyer.address));
  assert.equal(await vault.totalSupply(), scale);
  assert.equal(await first.balanceOf(buyer.address), firstBefore - units[0]);
  assert.equal(await second.balanceOf(buyer.address), secondBefore - units[1]);
  for (let i = 0; i < 2; i++) {
    const [held, owed, surplus] = await vault.reserveAt(i);
    assert.deepEqual([held, owed, surplus], [units[i], units[i], 0n]);
  }
  await sent('transferShare', vault.transfer(deployer.address, 4n * 10n ** 17n));
  await sent('pauseIssuance', factory.pauseIssuance(vaultAddress));
  let pausedMint = false;
  try { await vault.mint.staticCall(granularity, buyer.address); } catch { pausedMint = true; }
  assert.equal(pausedMint, true);
  await sent('redeemTransferred', vault.connect(deployer).redeem(4n * 10n ** 17n, deployer.address));
  await sent('redeemRemainder', vault.redeem(6n * 10n ** 17n, buyer.address));
  assert.equal(await vault.totalSupply(), 0n);
  assert.equal(await vault.balanceOf(buyer.address), 0n);
  assert.equal(await vault.balanceOf(deployer.address), 0n);
  assert.equal(await first.balanceOf(vaultAddress), 0n);
  assert.equal(await second.balanceOf(vaultAddress), 0n);
  assert.equal(await first.balanceOf(buyer.address), firstBefore - 40_000n);
  assert.equal(await second.balanceOf(buyer.address), secondBefore - 40_000n);
  record.accepted = true;
  record.finalSupply = '0';
  record.finalVaultReserves = ['0', '0'];
  fs.writeFileSync(evidencePath, JSON.stringify(record, null, 2), { flag: 'wx', mode: 0o600 });
  console.log(JSON.stringify({ result: 'PR05_TESTNET_ACCEPTED', factory: factoryAddress,
    vault: vaultAddress, transactions: record.transactions.map(({ label, hash, status }) =>
      ({ label, hash, status })) }, null, 2));
}

main().catch(error => { console.error(`PR05 stopped: ${error.shortMessage || error.message}`); process.exitCode = 1; });
