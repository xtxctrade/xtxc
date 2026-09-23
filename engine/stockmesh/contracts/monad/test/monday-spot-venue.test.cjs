const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const ganache = require('ganache');
const { ethers } = require('ethers');

const root = path.join(__dirname, '..');
const sources = ['src/StockMeshExecutor.sol', 'src/MondaySpotVenue.sol',
  'test/MockAtomicVenue.sol', 'test/MockMondaySpot.sol'];
const input = {
  language: 'Solidity',
  sources: Object.fromEntries(sources.map(file => [path.basename(file),
    { content: fs.readFileSync(path.join(root, file), 'utf8') }])),
  settings: { evmVersion: 'shanghai', viaIR: true,
    optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } },
};
const compiled = JSON.parse(solc.compile(JSON.stringify(input)));
const errors = (compiled.errors || []).filter(e => e.severity === 'error');
assert.deepEqual(errors, [], errors.map(e => e.formattedMessage).join('\n'));

function make(file, name, signer) {
  const c = compiled.contracts[file][name];
  return new ethers.ContractFactory(c.abi, `0x${c.evm.bytecode.object}`, signer);
}
async function rejected(promise, label) {
  let failed = false;
  try { const tx = await promise; await tx.wait(); } catch { failed = true; }
  assert.equal(failed, true, label);
}

(async () => {
  const chain = ganache.provider({ chain: { chainId: 143 }, logging: { quiet: true },
    wallet: { totalAccounts: 4 } });
  const provider = new ethers.BrowserProvider(chain);
  const governor = await provider.getSigner(0);
  const user = await provider.getSigner(1);
  const fees = await provider.getSigner(2);
  const outsider = await provider.getSigner(3);
  const usdc = await make('MockAtomicVenue.sol', 'MockToken', governor).deploy('USD Coin', 'USDC');
  const stock = await make('MockAtomicVenue.sol', 'MockToken', governor).deploy('BIL', 'aBIL');
  const pool = await make('MockMondaySpot.sol', 'MockMondayPool', governor).deploy();
  await Promise.all([usdc.waitForDeployment(), stock.waitForDeployment(), pool.waitForDeployment()]);
  const factory = await make('MockMondaySpot.sol', 'MockMondayFactory', governor).deploy(await pool.getAddress());
  await factory.waitForDeployment();
  const router = await make('MockMondaySpot.sol', 'MockMondayRouter', governor).deploy(await factory.getAddress());
  await router.waitForDeployment();
  const executor = await make('StockMeshExecutor.sol', 'StockMeshExecutor', governor)
    .deploy(await usdc.getAddress(), await fees.getAddress());
  await executor.waitForDeployment();
  const adapter = await make('MondaySpotVenue.sol', 'MondaySpotVenue', governor).deploy(
    await executor.getAddress(), await usdc.getAddress(), await stock.getAddress(),
    await router.getAddress(), await pool.getAddress(), 3000);
  await adapter.waitForDeployment();
  assert.equal(await adapter.poolFee(), 3000n);
  await rejected(make('MondaySpotVenue.sol', 'MondaySpotVenue', governor).deploy(
    await executor.getAddress(), await usdc.getAddress(), await stock.getAddress(),
    await router.getAddress(), await outsider.getAddress(), 3000), 'wrong pool constructor');

  const productId = ethers.id('eip155:143:erc20:anchored:bil');
  await (await executor.configureProduct(productId, await stock.getAddress())).wait();
  await (await executor.configureVenue(productId, await adapter.getAddress(), true)).wait();
  await (await usdc.mint(await user.getAddress(), 5_000_000n)).wait();
  await (await usdc.mint(await router.getAddress(), 5_000_000n)).wait();
  await (await stock.mint(await router.getAddress(), 5_000_000n)).wait();
  await (await usdc.connect(user).approve(await executor.getAddress(), 5_000_000n)).wait();
  const base = {
    productId, quoteDigest: ethers.id('monday-quote'), owner: await user.getAddress(),
    receiver: await user.getAddress(), stock: await stock.getAddress(), venue: await adapter.getAddress(),
    nonce: 1n, amountIn: 1_000_000n, maxDebit: 1_000_000n,
    minOutput: 999_950n, feeCap: 50n, deadline: 4_000_000_000n, buy: true,
  };
  await rejected(adapter.connect(outsider).swapExactIn(
    await usdc.getAddress(), await stock.getAddress(), 1n, 1n, await outsider.getAddress()),
  'adapter cannot be called outside executor');
  await (await executor.connect(user).execute(base)).wait();
  assert.equal(await stock.balanceOf(await user.getAddress()), 999_950n);
  assert.equal(await usdc.balanceOf(await fees.getAddress()), 50n);
  assert.equal(await usdc.allowance(await adapter.getAddress(), await router.getAddress()), 0n);
  assert.equal(await usdc.balanceOf(await adapter.getAddress()), 0n);

  await (await router.setMode(true, false, false)).wait();
  await rejected(executor.connect(user).execute({ ...base, nonce: 2n }), 'partial spend rejected');
  await (await router.setMode(false, true, false)).wait();
  await rejected(executor.connect(user).execute({ ...base, nonce: 2n }), 'wrong receiver rejected');
  await (await router.setMode(false, false, true)).wait();
  await rejected(executor.connect(user).execute({ ...base, nonce: 2n }), 'router failure rejected');
  await (await router.setMode(false, false, false)).wait();
  await (await stock.setTransferFeeBps(100n)).wait();
  await rejected(executor.connect(user).execute({ ...base, nonce: 2n, minOutput: 900_000n }),
    'fee-on-transfer output rejected');
  await (await stock.setTransferFeeBps(0n)).wait();
  assert.equal(await executor.nonceUsed(await user.getAddress(), 2n), false);
  assert.equal(await usdc.balanceOf(await adapter.getAddress()), 0n);

  await (await stock.connect(user).approve(await executor.getAddress(), 200_000n)).wait();
  await (await executor.connect(user).execute({ ...base, nonce: 3n, buy: false,
    amountIn: 200_000n, maxDebit: 200_000n, minOutput: 199_990n, feeCap: 10n })).wait();
  assert.equal(await usdc.balanceOf(await fees.getAddress()), 60n);
  assert.equal(await stock.allowance(await adapter.getAddress(), await router.getAddress()), 0n);
  assert.equal(await stock.balanceOf(await adapter.getAddress()), 0n);

  await (await factory.setPool(await outsider.getAddress())).wait();
  await rejected(executor.connect(user).execute({ ...base, nonce: 4n }), 'factory pool change rejected');
  assert.equal(await executor.nonceUsed(await user.getAddress(), 4n), false);
  await chain.disconnect();
  console.log('MondaySpotVenue: typed BIL pair, buy/sell, fee, router failure, dust, pool pin PASS');
})().catch(e => { console.error(e); process.exitCode = 1; });
