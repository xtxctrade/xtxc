const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const { ethers } = require('ethers');

const RPC = 'https://testnet-rpc.monad.xyz';
const CHAIN_ID = 10143n;
const root = path.join(__dirname, '..');
const sources = ['StockMeshExecutor.sol', 'MetropolisDemoAssets.sol'];
const input = { language: 'Solidity', sources: Object.fromEntries(sources.map(name =>
  [name, { content: fs.readFileSync(path.join(root, 'src', name), 'utf8') }])),
  settings: { evmVersion: 'shanghai', viaIR: true, optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['abi'] } } } };
const compiled = JSON.parse(solc.compile(JSON.stringify(input)));
const errors = (compiled.errors || []).filter(item => item.severity === 'error');
if (errors.length) throw new Error(errors.map(item => item.formattedMessage).join('\n'));
const abi = (file, name) => compiled.contracts[file][name].abi;
const tokenAbi = abi('MetropolisDemoAssets.sol', 'MetropolisDemoToken');
const venueAbi = abi('MetropolisDemoAssets.sol', 'MetropolisDemoVenue');
const executorAbi = abi('StockMeshExecutor.sol', 'StockMeshExecutor');
const txs = [];

function key(pathname) {
  if (!pathname || !path.isAbsolute(pathname)) throw new Error('Absolute test-only key path required');
  const stat = fs.statSync(pathname);
  if ((stat.mode & 0o077) !== 0) throw new Error('Test-only key file must be private');
  const value = fs.readFileSync(pathname, 'utf8').trim();
  if (!/^0x[0-9a-fA-F]{64}$/.test(value)) throw new Error('Invalid test-only key');
  return value;
}
async function send(label, txPromise) {
  const tx = await txPromise;
  console.log(JSON.stringify({ step: label, state: 'SUBMITTED', txHash: tx.hash }));
  const receipt = await tx.wait();
  if (receipt.status !== 1) throw new Error(`${label} failed: ${tx.hash}`);
  txs.push({ step: label, hash: tx.hash, blockNumber: receipt.blockNumber,
    gasUsed: receipt.gasUsed.toString() });
  return receipt;
}
function selectQuote(quotes) {
  return quotes.reduce((best, item) => !best || item.output > best.output ? item : best, null);
}
function parseExecuted(executor, receipt) {
  for (const log of receipt.logs) {
    if (log.address.toLowerCase() !== executor.target.toLowerCase()) continue;
    try { const parsed = executor.interface.parseLog(log); if (parsed?.name === 'Executed') return parsed.args; }
    catch { /* unrelated event */ }
  }
  throw new Error(`Missing Executed event: ${receipt.hash}`);
}

async function main() {
  if (process.argv.includes('--broadcast') === false
    || process.env.XTXC_TESTNET_DEPLOY_APPROVED !== 'METROPOLIS_TESTNET_ONLY')
    throw new Error('Explicit testnet-only broadcast marker required');
  const manifestPath = process.env.XTXC_TESTNET_MANIFEST_PATH;
  const evidencePath = process.env.XTXC_TESTNET_SMOKE_PATH;
  const buyerKeyPath = process.env.XTXC_TESTNET_BUYER_KEY_FILE;
  if (!manifestPath || !evidencePath || !buyerKeyPath
    || ![manifestPath, evidencePath, buyerKeyPath].every(path.isAbsolute)
    || fs.existsSync(evidencePath)) throw new Error('Manifest, fresh evidence, and buyer key paths required');
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  assert.equal(manifest.chainId, Number(CHAIN_ID));
  assert.equal(manifest.assetClass, 'DEMO_NO_EQUITY_RIGHTS');
  const provider = new ethers.JsonRpcProvider(RPC);
  assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
  const deployer = new ethers.Wallet(key(process.env.XTXC_TESTNET_DEPLOYER_KEY_FILE), provider);
  assert.equal(deployer.address.toLowerCase(), manifest.deployer.toLowerCase());
  if (!fs.existsSync(buyerKeyPath)) {
    const created = ethers.Wallet.createRandom();
    fs.writeFileSync(buyerKeyPath, created.privateKey + '\n', { flag: 'wx', mode: 0o600 });
  }
  const buyer = new ethers.Wallet(key(buyerKeyPath), provider);
  const cash = new ethers.Contract(manifest.cash, tokenAbi, buyer);
  const stock = new ethers.Contract(manifest.stock, tokenAbi, buyer);
  const executor = new ethers.Contract(manifest.executor, executorAbi, buyer);
  const venues = [manifest.venueA, manifest.venueB].map(address =>
    new ethers.Contract(address, venueAbi, buyer));
  for (const address of [manifest.cash, manifest.stock, manifest.executor,
    manifest.venueA, manifest.venueB]) {
    if ((await provider.getCode(address)) === '0x') throw new Error(`No testnet code: ${address}`);
  }
  assert.equal(await cash.symbol(), 'dUSD');
  assert.equal(await stock.symbol(), 'dNVDA');
  assert.equal(await executor.CHAIN_ID(), CHAIN_ID);
  assert.equal((await executor.productStock(manifest.productId)).toLowerCase(), manifest.stock.toLowerCase());
  for (const venue of venues) assert.equal(await executor.venueEnabled(manifest.productId, venue.target), true);

  if ((await provider.getBalance(buyer.address)) < ethers.parseEther('0.05'))
    await send('fundBuyerGas', deployer.sendTransaction({ to: buyer.address,
      value: ethers.parseEther('0.25') }));
  const cashBefore = await cash.balanceOf(buyer.address);
  const stockBefore = await stock.balanceOf(buyer.address);
  const claimAmount = cashBefore === 0n ? 1_000_000_000n : 0n;
  if (claimAmount !== 0n) await send('claimDemoCash', cash.claim());
  const amountIn = 100_000_000n;
  const feeBuy = amountIn / 20_000n;
  await send('approveDemoCash', cash.approve(manifest.executor, amountIn));
  const buyQuotes = await Promise.all(venues.map(async venue => ({
    venue, output: await venue.quoteExactIn(manifest.cash, amountIn - feeBuy),
  })));
  const buyBest = selectQuote(buyQuotes);
  const block = await provider.getBlock('latest');
  const buyNonce = BigInt(ethers.hexlify(ethers.randomBytes(16)));
  const buyOrder = { productId: manifest.productId, quoteDigest: ethers.id(`metropolis-buy:${buyer.address}:${buyNonce}`),
    owner: buyer.address, receiver: buyer.address, stock: manifest.stock, venue: buyBest.venue.target,
    nonce: buyNonce, amountIn, maxDebit: amountIn, minOutput: buyBest.output * 995n / 1000n,
    feeCap: feeBuy, deadline: BigInt(block.timestamp + 600), buy: true };
  const buyReceipt = await send('buyDemoStock', executor.execute(buyOrder));
  const buyEvent = parseExecuted(executor, buyReceipt);
  const bought = await stock.balanceOf(buyer.address) - stockBefore;
  assert.equal(bought, buyEvent.walletOutput);
  assert.equal(await cash.balanceOf(buyer.address), cashBefore + claimAmount - amountIn);
  assert.equal(buyEvent.platformFee, feeBuy);
  assert.equal(await executor.nonceUsed(buyer.address, buyNonce), true);

  let replayRejected = false;
  try { await executor.execute.staticCall(buyOrder); } catch { replayRejected = true; }
  assert.equal(replayRejected, true, 'replay must reject');
  await send('approveRollbackInput', cash.approve(manifest.executor, amountIn));
  const failNonce = BigInt(ethers.hexlify(ethers.randomBytes(16)));
  const beforeFailCash = await cash.balanceOf(buyer.address);
  const beforeFailStock = await stock.balanceOf(buyer.address);
  const failOrder = { ...buyOrder, nonce: failNonce,
    quoteDigest: ethers.id(`metropolis-fail:${buyer.address}:${failNonce}`),
    minOutput: buyBest.output + 1_000_000_000n };
  const failTx = await executor.execute(failOrder, { gasLimit: 750_000 });
  console.log(JSON.stringify({ step: 'slippageRollback', state: 'SUBMITTED', txHash: failTx.hash }));
  let failReceipt;
  try { failReceipt = await failTx.wait(); }
  catch (error) { failReceipt = error.receipt || await provider.getTransactionReceipt(failTx.hash); }
  assert.equal(failReceipt?.status, 0, 'slippage transaction must revert');
  txs.push({ step: 'slippageRollback', hash: failTx.hash, blockNumber: failReceipt.blockNumber,
    gasUsed: failReceipt.gasUsed.toString(), status: 0 });
  assert.equal(await cash.balanceOf(buyer.address), beforeFailCash);
  assert.equal(await stock.balanceOf(buyer.address), beforeFailStock);
  assert.equal(await executor.nonceUsed(buyer.address, failNonce), false);

  const sellAmount = bought / 2n;
  await send('approveDemoStock', stock.approve(manifest.executor, sellAmount));
  const sellQuotes = await Promise.all(venues.map(async venue => ({
    venue, output: await venue.quoteExactIn(manifest.stock, sellAmount),
  })));
  const sellBest = selectQuote(sellQuotes);
  const sellFee = sellBest.output / 20_000n;
  const sellNonce = BigInt(ethers.hexlify(ethers.randomBytes(16)));
  const sellOrder = { ...buyOrder, quoteDigest: ethers.id(`metropolis-sell:${buyer.address}:${sellNonce}`),
    venue: sellBest.venue.target, nonce: sellNonce, amountIn: sellAmount,
    maxDebit: sellAmount, minOutput: (sellBest.output - sellFee) * 995n / 1000n,
    feeCap: sellFee, deadline: BigInt((await provider.getBlock('latest')).timestamp + 600), buy: false };
  const beforeSellCash = await cash.balanceOf(buyer.address);
  const sellReceipt = await send('sellDemoStock', executor.execute(sellOrder));
  const sellEvent = parseExecuted(executor, sellReceipt);
  assert.equal(await cash.balanceOf(buyer.address) - beforeSellCash, sellEvent.walletOutput);
  assert.equal(await stock.balanceOf(buyer.address), stockBefore + bought - sellAmount);
  assert.equal(await executor.nonceUsed(buyer.address, sellNonce), true);
  const evidence = { chainId: Number(CHAIN_ID), assetClass: manifest.assetClass,
    buyer: buyer.address, deployer: deployer.address, manifest: manifestPath,
    buyVenue: buyBest.venue.target, sellVenue: sellBest.venue.target,
    buyOutputAtoms: bought.toString(), sellOutputAtoms: sellEvent.walletOutput.toString(),
    cashAfterAtoms: (await cash.balanceOf(buyer.address)).toString(),
    stockAfterAtoms: (await stock.balanceOf(buyer.address)).toString(),
    replayRejected, rollbackNonceUnused: true, transactions: txs };
  fs.writeFileSync(evidencePath, JSON.stringify(evidence, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
  console.log(JSON.stringify(evidence, null, 2));
}

main().catch(error => { console.error(error.message); process.exitCode = 1; });
