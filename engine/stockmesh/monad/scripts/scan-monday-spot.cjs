// Read-only cohort scan. No keys, signatures, transactions, or implicit retry.
const fs = require('node:fs');
const path = require('node:path');
const { Interface } = require('ethers');

const endpoint = process.env.MONAD_RPC_URL || 'https://rpc.monad.xyz';
if (!endpoint.startsWith('https://')) throw new Error('HTTPS Monad RPC required');
const catalog = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'catalog', 'registry.v1.json'), 'utf8'));
const factory = '0xC1e98D0A2a58fB8aBd10ccc30a58efff4080Aa21';
const multicall = '0xca11bde05977b3631167028862be2a173976ca11';
const quoter = '0xB97eCD41Aef0F842E773C8F9905919cDE49880C9';
const fees = [100, 300, 500, 3000, 10000];
const factoryAbi = new Interface(['function getPool(address,address,uint24) view returns (address)']);
const multiAbi = new Interface(['function aggregate3((address target,bool allowFailure,bytes callData)[] calls) payable returns ((bool success,bytes returnData)[])']);
const quoteAbi = new Interface(['function quoteExactInputSingle((address tokenIn,address tokenOut,uint256 amountIn,uint24 fee,uint160 sqrtPriceLimitX96) params) returns (uint256 amountOut,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate)']);
const poolAbi = new Interface(['function liquidity() view returns (uint128)']);
const tokenAbi = new Interface(['function balanceOf(address) view returns (uint256)']);
const zero = '0x0000000000000000000000000000000000000000';
let used = 0;
async function rpc(method, params) {
  if (++used > 24) throw new Error('read-only RPC call cap');
  const response = await fetch(endpoint, { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: used, method, params }), signal: AbortSignal.timeout(8000) });
  if (!response.ok) throw new Error(`Monad RPC HTTP ${response.status}`);
  const body = await response.json();
  if (body.id !== used || body.error || body.result == null) throw new Error('Monad RPC response invalid');
  return body.result;
}
async function main() {
  const chain = await rpc('eth_chainId', []);
  if (chain !== '0x8f') throw new Error('wrong chain');
  const head = await rpc('eth_getBlockByNumber', ['finalized', false]);
  if (!head?.number || !head?.hash) throw new Error('finalized block missing');
  const calls = catalog.tokenObservations.flatMap(t => fees.map(fee => ({
    token: t, fee,
    call: { target: factory, allowFailure: true,
      callData: factoryAbi.encodeFunctionData('getPool', [catalog.chain.usdc, t.tokenAddress, fee]) },
  })));
  const found = [];
  let successful = 0;
  for (let start = 0; start < calls.length; start += 64) {
    const group = calls.slice(start, start + 64);
    const data = multiAbi.encodeFunctionData('aggregate3', [group.map(x => x.call)]);
    const raw = await rpc('eth_call', [{ to: multicall, data }, head.number]);
    const decoded = multiAbi.decodeFunctionResult('aggregate3', raw)[0];
    if (decoded.length !== group.length) throw new Error('multicall length mismatch');
    for (let i = 0; i < group.length; i++) {
      if (!decoded[i].success) throw new Error(`getPool failed at call ${start + i}`);
      const pool = factoryAbi.decodeFunctionResult('getPool', decoded[i].returnData)[0];
      successful++;
      if (pool.toLowerCase() !== zero) found.push({ instrumentId: group[i].token.instrumentId,
        token: group[i].token.tokenAddress, fee: group[i].fee, pool });
    }
  }
  for (const foundPool of found) {
    const token = catalog.tokenObservations.find(t => t.tokenAddress.toLowerCase() === foundPool.token.toLowerCase());
    const poolLiquidity = await rpc('eth_call', [{ to: foundPool.pool,
      data: poolAbi.encodeFunctionData('liquidity', []) }, head.number]);
    foundPool.activeLiquidity = poolAbi.decodeFunctionResult('liquidity', poolLiquidity)[0].toString();
    const usdcBalance = await rpc('eth_call', [{ to: catalog.chain.usdc,
      data: tokenAbi.encodeFunctionData('balanceOf', [foundPool.pool]) }, head.number]);
    foundPool.usdcBalanceAtoms = tokenAbi.decodeFunctionResult('balanceOf', usdcBalance)[0].toString();
    const tokenBalance = await rpc('eth_call', [{ to: token.tokenAddress,
      data: tokenAbi.encodeFunctionData('balanceOf', [foundPool.pool]) }, head.number]);
    foundPool.tokenBalanceAtoms = tokenAbi.decodeFunctionResult('balanceOf', tokenBalance)[0].toString();
    const directions = [
      { name: 'oneUsdcToToken', tokenIn: catalog.chain.usdc, tokenOut: token.tokenAddress, amountIn: 1_000_000n },
      { name: 'oneHundredthTokenToUsdc', tokenIn: token.tokenAddress, tokenOut: catalog.chain.usdc,
        amountIn: 10n ** BigInt(token.tokenDecimals) / 100n },
    ];
    foundPool.readOnlyQuotes = {};
    for (const direction of directions) {
      try {
        const data = quoteAbi.encodeFunctionData('quoteExactInputSingle', [[
          direction.tokenIn, direction.tokenOut, direction.amountIn, foundPool.fee, 0,
        ]]);
        const raw = await rpc('eth_call', [{ to: quoter, data }, head.number]);
        const result = quoteAbi.decodeFunctionResult('quoteExactInputSingle', raw);
        foundPool.readOnlyQuotes[direction.name] = { inputAtoms: direction.amountIn.toString(),
          outputAtoms: result[0].toString(), gasEstimate: result[3].toString() };
      } catch {
        foundPool.readOnlyQuotes[direction.name] = { error: 'quote unavailable' };
      }
    }
  }
  const same = await rpc('eth_getBlockByNumber', [head.number, false]);
  if (same?.hash !== head.hash) throw new Error('finalized block changed during scan');
  process.stdout.write(JSON.stringify({ schema: 'xtxc.monad.monday-spot-scan/v1',
    chainId: 143, blockNumber: head.number, blockHash: head.hash,
    tokenCount: catalog.tokenObservations.length, feeTiers: fees,
    successfulCalls: successful, pools: found, rpcCalls: used }) + '\n');
}
main().catch(e => { console.error(e.message); process.exitCode = 1; });
