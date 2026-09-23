// Read-only, bounded public-order probe. Never signs or submits transactions.
const { Interface } = require('ethers');

const endpoint = process.env.MONAD_RPC_URL || 'https://rpc.monad.xyz';
if (!endpoint.startsWith('https://')) throw Error('HTTPS Monad RPC required');
const router = '0x2f903ac6ddaf57eadcbbc46adc3ad739c3506a2d';
const abi = new Interface([
  'event DepositAndMarketBuy(bytes32 indexed depositOpId,bytes32 indexed orderId,address indexed user,address depositToken,uint96 depositAmount,address stockToken,int96 usdAmount)',
  'event DepositStockAndMarketSell(bytes32 indexed orderId,address indexed user,address indexed stockToken,uint256 depositAmount,int96 quantity)',
]);
let used = 0;
async function rpc(method, params) {
  if (++used > 5) throw Error('read-only RPC budget exceeded');
  const response = await fetch(endpoint, { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: used, method, params }), signal: AbortSignal.timeout(10000) });
  if (!response.ok) throw Error(`Monad ${method} HTTP ${response.status}`);
  const result = await response.json();
  if (result.id !== used || result.error || result.result == null) throw Error('Monad RPC response invalid');
  return result.result;
}
async function main() {
  if (await rpc('eth_chainId', []) !== '0x8f') throw Error('wrong chain');
  const head = await rpc('eth_getBlockByNumber', ['finalized', false]);
  if (!head?.number || !head?.hash) throw Error('finalized block missing');
  const last = BigInt(head.number);
  const span = Number(process.env.MONAD_PROBE_SPAN || 32);
  if (!Number.isInteger(span) || span < 1 || span > 1000) throw Error('invalid bounded span');
  const first = last > BigInt(span) ? last - BigInt(span) : 0n;
  const logs = await rpc('eth_getLogs', [{ address: router,
    fromBlock: `0x${first.toString(16)}`, toBlock: head.number,
    topics: [[abi.getEvent('DepositAndMarketBuy').topicHash,
      abi.getEvent('DepositStockAndMarketSell').topicHash]] }]);
  if (!Array.isArray(logs) || logs.length > 2000) throw Error('log bound exceeded');
  const sample = logs.slice(0, 16).map(log => {
    const event = abi.parseLog(log);
    if (!event || log.removed) throw Error('unknown or removed event');
    return { name: event.name, transactionHash: log.transactionHash,
      orderId: event.args.orderId,
      stockToken: event.args.stockToken,
      inputAtoms: (event.name === 'DepositAndMarketBuy' ? event.args.depositAmount : event.args.depositAmount).toString(),
      requestedAmount: (event.name === 'DepositAndMarketBuy' ? event.args.usdAmount : event.args.quantity).toString() };
  });
  const same = await rpc('eth_getBlockByNumber', [head.number, false]);
  if (same?.hash !== head.hash) throw Error('finalized block changed');
  process.stdout.write(JSON.stringify({ schema: 'xtxc.monad.monday-rwa-probe/v1',
    blockNumber: head.number, blockHash: head.hash,
    fromBlock: `0x${first.toString(16)}`, observedOrders: logs.length,
    sample, rpcCalls: used }) + '\n');
}
main().catch(e => { console.error(e.message); process.exitCode = 1; });
