// The addon for this platform, in this order: the one the release workflow
// built (lucivy.<platform>.node next to this file), the platform package
// npm installed (lucivy-linux-x64-gnu, lucivy-darwin-arm64, ...), or a
// local build (`npm run build` leaves lucivy.node here).
const { platform, arch } = process;

function platformKey() {
  if (platform === 'linux') return `linux-${arch}-gnu`;
  if (platform === 'darwin') return `darwin-${arch}`;
  if (platform === 'win32') return `win32-${arch}-msvc`;
  return `${platform}-${arch}`;
}

function load() {
  const key = platformKey();
  // The Windows package is `lucivy-windows-x64`: npm's spam filter refused
  // the napi-style `lucivy-win32-x64-msvc` at first publish.
  const pkg = key === 'win32-x64-msvc' ? 'lucivy-windows-x64' : `lucivy-${key}`;
  const candidates = [
    `./lucivy.${key}.node`,
    pkg,
    `./lucivy.node`,
  ];
  const errors = [];
  for (const candidate of candidates) {
    try {
      return require(candidate);
    } catch (e) {
      // Missing: try the next. Present but not loadable (a build for
      // another platform, a broken file): say so, and try the next too.
      errors.push(`${candidate}: ${String(e.message).split('\n')[0]}`);
    }
  }
  throw new Error(
    `lucivy: no addon loads on ${platform}-${arch}.\n` +
    `Prebuilt: Linux x64/arm64 (glibc >= 2.28), macOS x64/arm64, Windows x64. ` +
    `Elsewhere: clone https://github.com/L-Defraiteur/lucivy and run \`npm run build\` in bindings/nodejs.\n` +
    errors.map(e => '  ' + e).join('\n'),
  );
}

const { Index, BlobIndex, mergeStats } = load();

module.exports = { Index, BlobIndex, mergeStats };
