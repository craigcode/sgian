(() => {
  'use strict';
  const terminal = new Terminal({
    allowProposedApi: false,
    convertEol: false,
    cursorBlink: true,
    fontFamily: 'Cascadia Mono, Consolas, monospace',
    fontSize: 13,
    scrollback: 10000,
    theme: {
      background: '#101318', foreground: '#e5e7eb', cursor: '#7dd3fc',
      selectionBackground: '#31536e', black: '#1f2937', red: '#f87171',
      green: '#86efac', yellow: '#fde68a', blue: '#7dd3fc', magenta: '#c4b5fd',
      cyan: '#67e8f9', white: '#e5e7eb', brightBlack: '#6b7280'
    }
  });
  const fit = new FitAddon.FitAddon();
  terminal.loadAddon(fit);
  const search = new SearchAddon.SearchAddon();
  terminal.loadAddon(search);
  terminal.open(document.getElementById('terminal'));

  let restoring = false;
  let messages = Promise.resolve();
  const write = data => data ? new Promise(resolve => terminal.write(data, resolve)) : Promise.resolve();
  const postSize = () => {
    if (restoring) return;
    fit.fit();
    window.chrome.webview.postMessage({ type: 'resize', cols: terminal.cols, rows: terminal.rows });
  };
  terminal.onData(data => window.chrome.webview.postMessage({ type: 'input', data }));
  terminal.onResize(size => {
    if (!restoring) window.chrome.webview.postMessage({ type: 'resize', cols: size.cols, rows: size.rows });
  });
  window.chrome.webview.addEventListener('message', event => {
    const message = event.data || {};
    // Parse replay at the saved PTY dimensions before fitting the new view.
    // Serialize writes and searches so search sees all previously sent output.
    messages = messages.then(async () => {
      if (message.type === 'output') await write(message.data);
      if (message.type === 'reset') {
        restoring = true;
        try {
          terminal.reset();
          if (Number.isInteger(message.cols) && Number.isInteger(message.rows) && message.cols >= 2 && message.rows >= 2)
            terminal.resize(message.cols, message.rows);
          await write(message.data);
        } finally { restoring = false; postSize(); }
      }
      if (message.type === 'font-size') { terminal.options.fontSize = message.value; postSize(); }
      if (message.type === 'focus') terminal.focus();
      if (message.type === 'search') {
        const found = message.previous ? search.findPrevious(message.query || '') : search.findNext(message.query || '');
        window.chrome.webview.postMessage({ type: 'search-result', found });
      }
    }).catch(error => window.chrome.webview.postMessage({ type: 'error', message: String(error) }));
  });
  document.addEventListener('pointerdown', () => window.chrome.webview.postMessage({ type: 'activated' }));
  terminal.textarea.addEventListener('focus', () => window.chrome.webview.postMessage({ type: 'activated' }));
  new ResizeObserver(() => postSize()).observe(document.getElementById('terminal'));
  // Readiness means the terminal and host bridge are wired. Do not gate it on
  // requestAnimationFrame: WebView2 can throttle animation frames in a
  // non-interactive CI desktop even though the document is fully loaded.
  window.chrome.webview.postMessage({ type: 'ready', cols: terminal.cols, rows: terminal.rows });
  requestAnimationFrame(() => {
    postSize();
  });
})();
