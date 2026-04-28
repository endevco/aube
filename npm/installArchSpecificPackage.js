// Fetch the platform-matching @endevco/aube-<os>-<arch> sub-package at
// install time and hardlink (or copy) its three binaries into ./bin so
// npm's `bin` wrapper resolves directly to the native executable. The root
// package's bin targets are stable `./bin/<name>` paths so npm/npx can create
// shims without reading a rewritten package.json. On Windows, npm's generated
// `.cmd` shim needs a shebang target it can execute, so `./bin/<name>` is a
// tiny text file whose interpreter is the native `./bin/<name>.exe`. This
// mirrors https://www.npmjs.com/package/@jdxcode/mise — the preinstall
// approach avoids the JS shim at runtime and keeps `package-lock.json` free
// of six optional-dependency entries that are mostly skipped.
//
// Must stay CommonJS and use only the Node.js stdlib — it runs *before*
// any dependency is installed, so nothing from node_modules is reachable.

var spawn = require('child_process').spawn;
var path = require('path');
var fs = require('fs');

function main() {
    var pjson = require('./package.json');
    var version = pjson.version;

    // Nested `npm install` must stay local; otherwise it'd try to write
    // into the global prefix when the user ran `npm i -g @endevco/aube`.
    process.env.npm_config_global = 'false';

    var platform = process.platform; // darwin | linux | win32
    var arch = process.arch;         // arm64 | x64
    // On Linux, `process.report` exposes `glibcVersionRuntime` when the
    // runtime linked against glibc; its absence means musl (Alpine,
    // distroless-static). Same heuristic the `detect-libc` package uses.
    var suffix = '';
    if (platform === 'linux') {
        var glibc = '';
        try { glibc = process.report.getReport().header.glibcVersionRuntime || ''; } catch (_) {}
        if (!glibc) suffix = '-musl';
    }
    var subpkgName = '@endevco/aube-' + platform + '-' + arch + suffix;

    var npmCmd = platform === 'win32' ? 'npm.cmd' : 'npm';
    var args = ['install', '--no-save', '--no-package-lock', subpkgName + '@' + version];

    var cp = spawn(npmCmd, args, { stdio: 'inherit', shell: true });
    cp.on('close', function(code, signal) {
        // `code` is null when the child was killed by a signal (e.g.
        // OOM). `process.exit(null)` coerces to 0, which would tell
        // npm the preinstall succeeded — surface it as failure.
        if (signal || code === null) {
            console.error('[@endevco/aube] preinstall: `npm install ' + subpkgName + '` killed by ' + (signal || 'signal'));
            process.exit(1);
            return;
        }
        if (code !== 0) {
            process.exit(code);
            return;
        }
        try {
            linkSubpkgBins(subpkgName, platform);
            process.exit(0);
        } catch (e) {
            console.error('[@endevco/aube] preinstall failed: ' + (e && e.message ? e.message : e));
            process.exit(1);
        }
    });
}

function linkSubpkgBins(subpkgName, platform) {
    var subpkgJsonPath = require.resolve(subpkgName + '/package.json');
    var subpkg = JSON.parse(fs.readFileSync(subpkgJsonPath, 'utf8'));
    var subpkgDir = path.dirname(subpkgJsonPath);

    var binDir = path.resolve(__dirname, 'bin');
    try { fs.mkdirSync(binDir); } catch (e) { if (e.code !== 'EEXIST') throw e; }

    Object.keys(subpkg.bin).forEach(function(name) {
        var srcRel = subpkg.bin[name];
        var src = path.resolve(subpkgDir, srcRel);
        var destBasename = platform === 'win32' ? name + '.exe' : name;
        var dest = path.resolve(binDir, destBasename);

        try { fs.unlinkSync(dest); } catch (e) { if (e.code !== 'ENOENT') throw e; }
        try {
            // Hardlink is cheapest (same inode, no extra disk). On some
            // filesystems (cross-device, restricted sandboxes) hardlink
            // fails — fall through to a copy.
            fs.linkSync(src, dest);
        } catch (e) {
            fs.copyFileSync(src, dest);
        }
        if (platform !== 'win32') {
            try { fs.chmodSync(dest, 0o755); } catch (_) {}
        } else {
            var shim = path.resolve(binDir, name);
            try { fs.unlinkSync(shim); } catch (e) { if (e.code !== 'ENOENT') throw e; }
            fs.writeFileSync(shim, '#!' + dest.replace(/\\/g, '/') + '\n');
        }
    });
}

main();
