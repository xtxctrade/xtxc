const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const ganache = require('ganache');
const { ethers } = require('ethers');

const root = path.join(__dirname, '..');
const sources = ['StockMeshExecutor.sol', 'MetropolisDemoAssets.sol'];
const input = { language: 'Solidity', sources: Object.fromEntries(sources.map(name =>
  [name, { content: fs.readFileSync(path.join(root, 'src', name), 'utf8') }])),
  settings: { evmVersion: 'shanghai', viaIR: true, optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } } };
const compiled = JSON.parse(solc.compile(JSON.stringify(input)));
const errors = (compiled.errors || []).filter(item => item.severity === 'error');
assert.deepEqual(errors, [], errors.map(item => item.formattedMessage).join('\n'));
function contract(name, signer) {
  const file = name === 'StockMeshExecutor' ? 'StockMeshExecutor.sol' : 'MetropolisDemoAssets.sol';
  const item = compiled.contracts[file][name];
  return new ethers.ContractFactory(item.abi, `0x${item.evm.bytecode.object}`, signer);
}
async function rejected(promise, label) {
  let failed = false;
  try { const tx = await promise; await tx.wait(); } catch { failed = true; }
  assert.equal(failed, true, label);
}

(async () => {
  const chain = ganache.provider({ chain: { chainId: 10143 }, logging: { quiet: true },
    wallet: { totalAccounts: 3 } });
  const provider = new ethers.BrowserProvider(chain);
  const operator = await provider.getSigner(0);
  const buyer = await provider.getSigner(1);
  const feeWallet = await provider.getSigner(2);
  const token = contract('MetropolisDemoToken', operator);
  const usdc = await token.deploy('Demo USD, no cash claim', 'dUSD', true);
  const stock = await token.deploy('Demo NVDA, no equity claim', 'dNVDA', false);
  await Promise.all([usdc.waitForDeployment(), stock.waitForDeployment()]);
  const venue = await contract('MetropolisDemoVenue', operator).deploy(
    await usdc.getAddress(), await stock.getAddress(), 30n);
  const executor = await contract('StockMeshExecutor', operator).deploy(
    await usdc.getAddress(), await feeWallet.getAddress());
  await Promise.all([venue.waitForDeployment(), executor.waitForDeployment()]);
  assert.equal(await executor.CHAIN_ID(), 10143n);
  const productId = ethers.id('eip155:10143:erc20:demo:NVDA');
  await (await executor.configureProduct(productId, await stock.getAddress())).wait();
  await (await executor.configureVenue(productId, await venue.getAddress(), true)).wait();

  await (await usdc.mint(await operator.getAddress(), 100_000_000_000n)).wait();
  await (await stock.mint(await operator.getAddress(), 1_000_000_000n)).wait();
  await (await usdc.approve(await venue.getAddress(), 100_000_000_000n)).wait();
  await (await stock.approve(await venue.getAddress(), 1_000_000_000n)).wait();
  await (await venue.addLiquidity(100_000_000_000n, 1_000_000_000n)).wait();
  await (await usdc.connect(buyer).claim()).wait();
  await rejected(usdc.connect(buyer).claim(), 'faucet cannot be claimed twice');
  await (await usdc.connect(buyer).approve(await executor.getAddress(), 100_000_000n)).wait();

  const buyDebit = 100_000_000n;
  const buyFee = buyDebit / 20_000n;
  const buyQuote = await venue.quoteExactIn(await usdc.getAddress(), buyDebit - buyFee);
  const common = { productId, quoteDigest: ethers.id('demo-quote-1'),
    owner: await buyer.getAddress(), receiver: await buyer.getAddress(),
    stock: await stock.getAddress(), venue: await venue.getAddress(),
    maxDebit: buyDebit, deadline: 4_000_000_000n };
  const buy = { ...common, nonce: 1n, amountIn: buyDebit,
    minOutput: buyQuote, feeCap: buyFee, buy: true };
  await (await executor.connect(buyer).execute(buy)).wait();
  assert.equal(await stock.balanceOf(await buyer.getAddress()), buyQuote);
  assert.equal(await usdc.balanceOf(await feeWallet.getAddress()), buyFee);
  await rejected(executor.connect(buyer).execute(buy), 'buy replay blocked');
  await rejected(executor.connect(buyer).execute({ ...buy, nonce: 2n, minOutput: buyQuote + 1n }),
    'slippage rollback');
  assert.equal(await executor.nonceUsed(await buyer.getAddress(), 2n), false);

  const sellAmount = buyQuote / 2n;
  await (await stock.connect(buyer).approve(await executor.getAddress(), sellAmount)).wait();
  const grossSell = await venue.quoteExactIn(await stock.getAddress(), sellAmount);
  const sellFee = grossSell / 20_000n;
  const beforeCash = await usdc.balanceOf(await buyer.getAddress());
  await (await executor.connect(buyer).execute({ ...common,
    quoteDigest: ethers.id('demo-quote-2'), nonce: 3n,
    amountIn: sellAmount, maxDebit: sellAmount,
    minOutput: grossSell - sellFee, feeCap: sellFee, buy: false })).wait();
  assert.equal((await usdc.balanceOf(await buyer.getAddress())) - beforeCash, grossSell - sellFee);
  assert.equal(await stock.balanceOf(await buyer.getAddress()), buyQuote - sellAmount);
  await chain.disconnect();
  console.log('Metropolis demo: testnet chain, faucet, buy, wallet stock, sell, wallet cash, rollback, replay PASS');
})().catch(error => { console.error(error); process.exitCode = 1; });
