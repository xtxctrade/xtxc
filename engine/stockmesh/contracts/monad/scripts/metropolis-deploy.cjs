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
async function wait(tx) { return (await tx).wait(); }
async function deploy(signer, file, name, ...args) {
  const item = artifact(file, name);
  const instance = await new ethers.ContractFactory(item.abi, `0x${item.evm.bytecode.object}`, signer).deploy(...args);
  await instance.waitForDeployment();
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
  const key = process.env.XTXC_TESTNET_DEPLOYER_KEY;
  if (!key || !/^0x[0-9a-fA-F]{64}$/.test(key))
    throw new Error('Test-only deployer key must be supplied through the process environment');
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
  await wait(executor.configureProduct(productId, stockAddress));
  await wait(executor.configureVenue(productId, await venueA.getAddress(), true));
  await wait(executor.configureVenue(productId, await venueB.getAddress(), true));

  const setups = [
    { venue: venueA, cashAtoms: 100_000_000_000n, stockAtoms: 1_000_000_000n },
    { venue: venueB, cashAtoms: 120_000_000_000n, stockAtoms: 1_000_000_000n },
  ];
  await wait(cash.mint(signer.address, 220_000_000_000n));
  await wait(stock.mint(signer.address, 2_000_000_000n));
  for (const setup of setups) {
    const address = await setup.venue.getAddress();
    await wait(cash.approve(address, setup.cashAtoms));
    await wait(stock.approve(address, setup.stockAtoms));
    await wait(setup.venue.addLiquidity(setup.cashAtoms, setup.stockAtoms));
  }
  if (await executor.CHAIN_ID() !== TESTNET_CHAIN_ID
    || await executor.productStock(productId) !== stockAddress)
    throw new Error('Post-deployment verification failed');
  const manifest = {
    chainId: Number(TESTNET_CHAIN_ID), network: 'Monad Testnet', assetClass: 'DEMO_NO_EQUITY_RIGHTS',
    deployer: signer.address, cash: cashAddress, stock: stockAddress,
    executor: await executor.getAddress(),
    venueA: await venueA.getAddress(), venueB: await venueB.getAddress(), productId,
    transactions: {
      cash: cash.deploymentTransaction().hash, stock: stock.deploymentTransaction().hash,
      venueA: venueA.deploymentTransaction().hash, venueB: venueB.deploymentTransaction().hash,
      executor: executor.deploymentTransaction().hash,
    },
  };
  console.log(JSON.stringify(manifest, null, 2));
}

main().catch(error => { console.error(error.message); process.exitCode = 1; });
