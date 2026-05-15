fs.writeFileSync(path.join(execEnv.buildDir, 'package.json'), JSON.stringify({
  name: 'exec-pkg',
  version: '2.0.0',
  main: 'index.js'
}));

fs.writeFileSync(path.join(execEnv.buildDir, 'index.js'), "module.exports = 'exec ok';\n");
