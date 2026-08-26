// The addon for this platform: from the platform package npm installed
// (lucivy-linux-x64-gnu, lucivy-darwin-arm64, ...), or from a local build
// (`npm run build` leaves lucivy.node next to this file, the release
// workflow leaves lucivy.<platform>.node).
const { platform, arch } = process;

function platformKey() {
  if (platform === 'linux') return `linux-${arch}-gnu`;
  if (platform === 'darwin') return `darwin-${arch}`;
  if (platform === 'win32') return `win32-${arch}-msvc`;
  return `${platform}-${arch}`;
}

function load() {
  const key = platformKey();
  const candidates = [
    `./lucivy.node`,
    `./lucivy.${key}.node`,
    `lucivy-${key}`,
  ];
  const errors = [];
  for (const candidate of candidates) {
    try {
      return require(candidate);
    } catch (e) {
      if (e.code !== 'MODULE_NOT_FOUND') throw e;
      errors.push(`${candidate}: ${e.message.split('\n')[0]}`);
    }
  }
  throw new Error(
    `lucivy: no prebuilt addon for ${platform}-${arch} (looked for ${candidates.join(', ')}).\n` +
    `Prebuilt: Linux x64/arm64 (glibc >= 2.28), macOS x64/arm64, Windows x64. ` +
    `Elsewhere: clone https://github.com/L-Defraiteur/lucivy and run \`npm run build\` in bindings/nodejs.\n` +
    errors.join('\n'),
  );
}

const { Index, BlobIndex, mergeStats } = load();

module.exports = { Index, BlobIndex, mergeStats };
