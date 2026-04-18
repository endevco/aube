// Fetch the platform-matching @endevco/aube-<os>-<arch> sub-package at
// install time and hardlink (or copy) its three binaries into ./bin so
// npm's `bin` wrapper resolves directly to the native executable. This
// mirrors https://www.npmjs.com/package/@jdxcode/mise — the preinstall
// approach avoids the JS shim at runtime and keeps `package-lock.json`
// free of six optional-dependency entries that are mostly skipped.
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
    var subpkgName = '@endevco/aube-' + platform + '-' + arch;

    var npmCmd = platform === 'win32' ? 'npm.cmd' : 'npm';
    var args = ['install', '--no-save', '--no-package-lock', subpkgName + '@' + version];

    var cp = spawn(npmCmd, args, { stdio: 'inherit', shell: true });
    cp.on('close', function(code) {
        if (code !== 0) {
            process.exit(code);
            return;
        }
        try {
            linkSubpkgBins(subpkgName, platform, pjson);
            process.exit(0);
        } catch (e) {
            console.error('[@endevco/aube] preinstall failed: ' + (e && e.message ? e.message : e));
            process.exit(1);
        }
    });
}

function linkSubpkgBins(subpkgName, platform, pjson) {
    var subpkgJsonPath = require.resolve(subpkgName + '/package.json');
    var subpkg = JSON.parse(fs.readFileSync(subpkgJsonPath, 'utf8'));
    var subpkgDir = path.dirname(subpkgJsonPath);

    var binDir = path.resolve(__dirname, 'bin');
    try { fs.mkdirSync(binDir); } catch (e) { if (e.code !== 'EEXIST') throw e; }

    var rootPkgJsonPath = path.resolve(__dirname, 'package.json');
    var rootPkg = JSON.parse(fs.readFileSync(rootPkgJsonPath, 'utf8'));
    var rootPkgChanged = false;

    Object.keys(subpkg.bin).forEach(function(name) {
        var srcRel = subpkg.bin[name];
        var src = path.resolve(subpkgDir, srcRel);
        var destBasename = path.basename(srcRel);
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
        }

        // Keep root package.json's `bin` entry aligned with the filename
        // we actually wrote. Matters on Windows, where the sub-package's
        // bin ends in `.exe` — the root's original `./bin/<name>` entry
        // would leave the npm bin wrapper pointing at a missing file.
        var desiredBin = './bin/' + destBasename;
        if (rootPkg.bin[name] !== desiredBin) {
            rootPkg.bin[name] = desiredBin;
            rootPkgChanged = true;
        }
    });

    if (rootPkgChanged) {
        fs.writeFileSync(rootPkgJsonPath, JSON.stringify(rootPkg, null, 2) + '\n');
    }
}

main();
