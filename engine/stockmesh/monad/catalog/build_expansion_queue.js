// Mechanically maps the existing 1,074 display instruments to Monad discovery.
// An observed token or ticker is never elevated to an executable product.
const fs = require('fs');
const crypto = require('crypto');
const [presentationPath, monadPath, outputPath] = process.argv.slice(2);
if (!presentationPath || !monadPath || !outputPath) throw new Error('usage: build_expansion_queue.js PRESENTATION MONAD_CATALOG OUTPUT');
const presentationBytes = fs.readFileSync(presentationPath);
const presentation = JSON.parse(presentationBytes);
const monad = JSON.parse(fs.readFileSync(monadPath));
if (presentation.schema !== 'xtxc.stock-presentation/v1' || !Array.isArray(presentation.instruments)) {
  throw new Error('invalid presentation catalog');
}
if (presentation.instruments.length !== 1074 || monad.tokenObservations.length !== 112) {
  throw new Error('unexpected catalog baseline');
}
const byInstrument = new Map();
for (const token of monad.tokenObservations) {
  const key = token.instrumentId.toUpperCase();
  if (!byInstrument.has(key)) byInstrument.set(key, []);
  byInstrument.get(key).push({ issuer: token.issuer, issuerProductId: token.issuerProductId,
    tokenAddress: token.tokenAddress, chainId: token.chainId });
}
const seen = new Set();
const instrumentIds = presentation.instruments.map((item) => {
  const instrumentId = item.instrument.toUpperCase();
  if (!instrumentId || seen.has(instrumentId)) throw new Error('duplicate or empty instrument');
  seen.add(instrumentId);
  return instrumentId;
});
const monadMatches = instrumentIds.filter((id) => byInstrument.has(id))
  .map((instrumentId) => ({ instrumentId, observations: byInstrument.get(instrumentId) }));
const unmatchedMonad = [...byInstrument.entries()]
  .filter(([instrumentId]) => !seen.has(instrumentId))
  .map(([instrumentId, observations]) => ({ instrumentId, observations,
    status: 'IDENTITY_MAPPING_REVIEW' }));
const output = {
  schema: 'xtxc.monad.expansion-queue/v1',
  sourcePresentationSha256: crypto.createHash('sha256').update(presentationBytes).digest('hex'),
  displayInstruments: instrumentIds.length,
  observedInstrumentMatches: monadMatches.length,
  observedTokensMatched: monadMatches.reduce((sum, item) => sum + item.observations.length, 0),
  unmatchedMonadInstrumentCount: unmatchedMonad.length,
  unmatchedMonadTokens: unmatchedMonad,
  executableBuy: 0, executableSell: 0, walletDelivery: 0, etfEligible: 0,
  instrumentIds,
  monadMatches,
};
fs.writeFileSync(outputPath, JSON.stringify(output, null, 2) + '\n');
process.stdout.write(`queue=${output.displayInstruments} Monad-matched=${output.observedInstrumentMatches} tokens=${output.observedTokensMatched} unmatched-Monad=${output.unmatchedMonadInstrumentCount}; execution=0\n`);
