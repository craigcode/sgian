import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';

function terminalHarness() {
  const trace = [], sent = [], listeners = {};
  let resizeListener;
  let focusListener;
  class Terminal {
    cols = 80; rows = 24; options = {};
    textarea = { addEventListener(name, callback) { if (name === 'focus') focusListener = callback; } };
    loadAddon() {} open() {} onData() {} focus() { trace.push(['focus']); focusListener?.(); }
    onResize(callback) { resizeListener = callback; }
    reset() { trace.push(['reset']); }
    resize(cols, rows) { this.cols = cols; this.rows = rows; trace.push(['size', cols, rows]); resizeListener({ cols, rows }); }
    write(data, callback) { trace.push(['write', data, this.cols, this.rows]); setTimeout(() => { trace.push(['parsed', data]); callback(); }, 5); }
  }
  class FitAddon { fit() { trace.push(['fit']); } }
  class SearchAddon {
    findNext(query) { trace.push(['search', query]); return true; }
    findPrevious(query) { return this.findNext(query); }
  }
  vm.runInNewContext(readFileSync(new URL('../apps/windows/Sgian.Windows/Terminal/terminal.js', import.meta.url), 'utf8'), {
    Terminal, FitAddon: { FitAddon }, SearchAddon: { SearchAddon },
    ResizeObserver: class { observe() {} }, requestAnimationFrame() {},
    document: { getElementById() { return {}; }, addEventListener() {} },
    window: { chrome: { webview: { postMessage: value => sent.push(value), addEventListener: (name, fn) => { listeners[name] = fn; } } } },
  });
  return { trace, sent, send: data => listeners.message({ data }) };
}

test('replay uses saved PTY dimensions and finishes before live output and search', async () => {
  const { trace, sent, send } = terminalHarness();
  send({ type: 'reset', data: 'saved ANSI', cols: 120, rows: 40 });
  send({ type: 'output', data: 'live output' });
  send({ type: 'search', query: 'live output' });
  await new Promise(resolve => setTimeout(resolve, 50));
  assert.deepEqual(trace, [
    ['reset'], ['size', 120, 40], ['write', 'saved ANSI', 120, 40], ['parsed', 'saved ANSI'], ['fit'],
    ['write', 'live output', 120, 40], ['parsed', 'live output'], ['search', 'live output'],
  ]);
  assert.equal(sent.filter(message => message.type === 'resize').length, 1, 'no daemon resize during replay');
  assert.equal(sent.at(-1).found, true);
});

test('empty replay without dimensions still completes and does not steal focus', async () => {
  const { trace, send } = terminalHarness();
  send({ type: 'reset', data: '' });
  send({ type: 'search', query: 'missing' });
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.deepEqual(trace, [['reset'], ['fit'], ['search', 'missing']]);
});

test('host focus does not echo a second pane activation back to the native client', async () => {
  const { trace, sent, send } = terminalHarness();
  send({ type: 'focus' });
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.deepEqual(trace, [['focus']]);
  assert.equal(sent.some(message => message.type === 'activated'), false);
});
