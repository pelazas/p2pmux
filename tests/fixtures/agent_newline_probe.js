const { emitKeypressEvents } = require('readline');

emitKeypressEvents(process.stdin);
if (process.stdin.setRawMode) process.stdin.setRawMode(true);
process.stdout.write('READY\n');
process.stdin.on('keypress', (s, k) => {
  const kind = (k && k.name === 'enter')
    ? 'NEWLINE'
    : (k && k.name === 'return' && !k.shift && !k.meta)
      ? 'SUBMIT'
      : JSON.stringify(k);
  process.stdout.write(kind + '\n');
  if (k && k.ctrl && k.name === 'c') process.exit(0);
});
