const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const { ethers } = require('ethers');

// No mainnet endpoint, key, address or production artifact is used here.
const RPC = 'https://testnet-rpc.monad.xyz';
const TESTNET_CHAIN_ID = 10143n;
const root = path.join(__dirname, '..');
const sources = ['StockMeshExecutor.sol', 'MetropolisDemoAssets.sol'];
const input = { language: 'Solidity', sources: Object.fromEntries(sources.map(name =>
  [name, { content: fs.readFileSync(path.join(root, 'src', name), 'utf8') }])),
  settings: { evmVersion: 'shanghai', viaIR: true, optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } } };
const compiled = JSON.parse(solc.compile(JSON.stringify(input)));
const errors = (compiled.errors || []).filter(item => item.severity === 'error');
if (errors.length) throw new Error(errors.map(item => item.formattedMessage).join('\n'));
function artifact(file, name) { return compiled.contracts[file][name]; }
const sent = [];
async function wait(label, txPromise) {
  const tx = await txPromise;
  console.log(JSON.stringify({ step: label, state: 'SUBMITTED', txHash: tx.hash }));
  const receipt = await tx.wait();
  if (receipt.status !== 1) throw new Error(`${label} reverted: ${tx.hash}`);
  sent.push({ step: label, txHash: tx.hash, blockNumber: receipt.blockNumber });
  return receipt;
}
async function deploy(signer, file, name, ...args) {
  const item = artifact(file, name);
  const instance = await new ethers.ContractFactory(item.abi, `0x${item.evm.bytecode.object}`, signer).deploy(...args);
  const tx = instance.deploymentTransaction();
  console.log(JSON.stringify({ step: `deploy:${name}`, state: 'SUBMITTED', txHash: tx.hash }));
  await instance.waitForDeployment();
  const receipt = await tx.wait();
  if (receipt.status !== 1) throw new Error(`${name} deployment reverted: ${tx.hash}`);
  sent.push({ step: `deploy:${name}`, txHash: tx.hash, blockNumber: receipt.blockNumber,
    address: await instance.getAddress() });
  return instance;
}

async function main() {
  const provider = new ethers.JsonRpcProvider(RPC);
  const network = await provider.getNetwork();
  if (network.chainId !== TESTNET_CHAIN_ID) throw new Error('Monad Testnet chain ID mismatch; no deployment attempted');
  const broadcast = process.argv.includes('--broadcast');
  if (!broadcast) {
    console.log(JSON.stringify({ mode: 'READ_ONLY', chainId: Number(network.chainId),
      rpc: RPC, compiled: sources, next: 'Requires separate approval, test-only wallet funding, then --broadcast' }, null, 2));
    return;
  }
  if (process.argv.some(arg => arg.startsWith('--') && arg !== '--broadcast'))
    throw new Error('Unknown option');
  if (process.env.XTXC_TESTNET_DEPLOY_APPROVED !== 'METROPOLIS_TESTNET_ONLY')
    throw new Error('Explicit testnet deployment approval marker required');
  const manifestPath = process.env.XTXC_TESTNET_MANIFEST_PATH;
  if (!manifestPath || !path.isAbsolute(manifestPath) || fs.existsSync(manifestPath))
    throw new Error('A new absolute testnet manifest path is required before broadcast');
  const keyFile = process.env.XTXC_TESTNET_DEPLOYER_KEY_FILE;
  if (!keyFile || !path.isAbsolute(keyFile)) throw new Error('Absolute testnet-only key file is required');
  const keyStat = fs.statSync(keyFile);
  if ((keyStat.mode & 0o077) !== 0) throw new Error('Testnet key file must not be group/world accessible');
  const key = fs.readFileSync(keyFile, 'utf8').trim();
  if (!key || !/^0x[0-9a-fA-F]{64}$/.test(key))
    throw new Error('Invalid test-only deployer key file');
  const signer = new ethers.Wallet(key, provider);
  if (await provider.getBalance(signer.address) === 0n)
    throw new Error('No testnet MON for deployment gas');

  const demo = 'MetropolisDemoAssets.sol';
  const cash = await deploy(signer, demo, 'MetropolisDemoToken',
    'Demo USD, no cash claim', 'dUSD', true);
  const stock = await deploy(signer, demo, 'MetropolisDemoToken',
    'Demo NVDA, no equity claim', 'dNVDA', false);
  const cashAddress = await cash.getAddress();
  const stockAddress = await stock.getAddress();
  const venueA = await deploy(signer, demo, 'MetropolisDemoVenue', cashAddress, stockAddress, 30n);
  const venueB = await deploy(signer, demo, 'MetropolisDemoVenue', cashAddress, stockAddress, 40n);
  const executor = await deploy(signer, 'StockMeshExecutor.sol', 'StockMeshExecutor',
    cashAddress, signer.address);
  const productId = ethers.id('eip155:10143:erc20:demo:NVDA');
  await wait('configureProduct', executor.configureProduct(productId, stockAddress));
  await wait('configureVenue:A', executor.configureVenue(productId, await venueA.getAddress(), true));
  await wait('configureVenue:B', executor.configureVenue(productId, await venueB.getAddress(), true));

  const setups = [
    { venue: venueA, cashAtoms: 100_000_000_000n, stockAtoms: 1_000_000_000n },
    { venue: venueB, cashAtoms: 120_000_000_000n, stockAtoms: 1_000_000_000n },
  ];
  await wait('mint:dUSD', cash.mint(signer.address, 220_000_000_000n));
  await wait('mint:dNVDA', stock.mint(signer.address, 2_000_000_000n));
  for (const setup of setups) {
    const address = await setup.venue.getAddress();
    await wait(`approve:dUSD:${address}`, cash.approve(address, setup.cashAtoms));
    await wait(`approve:dNVDA:${address}`, stock.approve(address, setup.stockAtoms));
    await wait(`addLiquidity:${address}`, setup.venue.addLiquidity(setup.cashAtoms, setup.stockAtoms));
  }
  if (await executor.CHAIN_ID() !== TESTNET_CHAIN_ID
    || await executor.productStock(productId) !== stockAddress)
    throw new Error('Post-deployment verification failed');
  const manifest = {
    chainId: Number(TESTNET_CHAIN_ID), network: 'Monad Testnet', assetClass: 'DEMO_NO_EQUITY_RIGHTS',
    deployer: signer.address, cash: cashAddress, stock: stockAddress,
    executor: await executor.getAddress(),
    venueA: await venueA.getAddress(), venueB: await venueB.getAddress(), productId,
    deploymentTransactions: {
      cash: cash.deploymentTransaction().hash, stock: stock.deploymentTransaction().hash,
      venueA: venueA.deploymentTransaction().hash, venueB: venueB.deploymentTransaction().hash,
      executor: executor.deploymentTransaction().hash,
    },
    transactions: sent,
  };
  fs.writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  console.log(JSON.stringify(manifest, null, 2));
}

main().catch(error => { console.error(error.message); process.exitCode = 1; });
