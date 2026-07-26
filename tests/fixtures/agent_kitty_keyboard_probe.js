if (process.stdin.setRawMode) process.stdin.setRawMode(true);

const query = '\x1b[?u';
const reply = '\x1b[?0u';
const push = '\x1b[>1u';
const shiftedEnter = '\x1b[13;2u';
let input = '';
let phase = 'query';

process.stdout.write(query);
process.stdin.on('data', (chunk) => {
  input += chunk.toString('utf8');
  if (phase === 'query') {
    if (!input.includes(reply)) return;
    phase = 'shift-enter';
    input = '';
    process.stdout.write(push);
    process.stdout.write('KITTY_READY\n');
    return;
  }

  if (input.includes(shiftedEnter)) {
    process.stdout.write('KITTY_SHIFT_ENTER\n');
    process.exit(0);
  }
  if (input.length >= shiftedEnter.length) {
    process.stdout.write('KITTY_UNEXPECTED_INPUT\n');
    process.exit(1);
  }
});
