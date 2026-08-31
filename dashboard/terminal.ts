import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

type TerminalCallbacks = {
  onData: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
};

function createTerminal(element: HTMLElement, callbacks: TerminalCallbacks) {
  const terminal = new Terminal({
    allowProposedApi: false,
    convertEol: true,
    cursorBlink: true,
    disableStdin: false,
    fontFamily: "Berkeley Mono, SFMono-Regular, Menlo, monospace",
    fontSize: 12,
    scrollback: 5000,
    theme: {
      background: "#080b0f",
      foreground: "#d7dce4",
      cursor: "#b6f36b",
      selectionBackground: "#31411f",
    },
  });
  const fit = new FitAddon();
  terminal.loadAddon(fit);
  terminal.open(element);
  fit.fit();
  terminal.onData(callbacks.onData);
  terminal.onResize(({ cols, rows }) => callbacks.onResize(cols, rows));
  const observer = new ResizeObserver(() => fit.fit());
  observer.observe(element);
  return {
    focus: () => terminal.focus(),
    reset: () => terminal.reset(),
    write: (data: string | Uint8Array) => terminal.write(data),
    dispose: () => {
      observer.disconnect();
      terminal.dispose();
    },
  };
}

declare global {
  interface Window {
    PidMeshTerminal: {
      create: typeof createTerminal;
    };
  }
}

window.PidMeshTerminal = { create: createTerminal };
