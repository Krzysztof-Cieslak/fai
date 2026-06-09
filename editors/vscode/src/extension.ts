import * as vscode from "vscode";
import {
  LanguageClient,
  type LanguageClientOptions,
  type ServerOptions,
} from "vscode-languageclient/node";

// One language server per workspace folder. Fai's `fai lsp` is single-root (it
// opens one warm session for the root it is given and does not read the LSP
// `rootUri`/`workspaceFolders`), so multi-root support lives here: the client
// launches one `fai lsp --project <folder>` process per folder and routes each
// document to the client whose folder contains it.
const clients = new Map<string, LanguageClient>();

// All clients share one output channel (and use it for tracing too), so a
// multi-root window has a single "Fai Language Server" view.
let outputChannel: vscode.OutputChannel | undefined;

function serverOptionsFor(folder: vscode.WorkspaceFolder): ServerOptions {
  const config = vscode.workspace.getConfiguration("fai", folder.uri);
  const command = config.get<string>("server.path", "fai");
  const extraArgs = config.get<string[]>("server.args", []);
  // The server takes its workspace root from `--project`, not the LSP
  // handshake; cwd is set too as a belt-and-suspenders default.
  const args = ["lsp", "--project", folder.uri.fsPath, ...extraArgs];
  return { command, args, options: { cwd: folder.uri.fsPath } };
}

function clientOptionsFor(folder: vscode.WorkspaceFolder): LanguageClientOptions {
  // Confine each client to its own folder so that, in a multi-root window, one
  // server handles one folder's documents. The protocol document selector takes
  // a string glob; normalize to forward slashes so it matches on Windows too.
  const glob = `${folder.uri.fsPath.replace(/\\/g, "/")}/**/*`;
  const config = vscode.workspace.getConfiguration("fai", folder.uri);
  return {
    documentSelector: [{ scheme: "file", language: "fai", pattern: glob }],
    workspaceFolder: folder,
    outputChannel,
    traceOutputChannel: outputChannel,
    // Server-side settings passed at the LSP handshake.
    initializationOptions: { examples: config.get<boolean>("examples", true) },
  };
}

async function startClient(folder: vscode.WorkspaceFolder): Promise<void> {
  const key = folder.uri.toString();
  if (clients.has(key)) {
    return;
  }
  // The client id ("fai") is what vscode-languageclient reads `fai.trace.server`
  // from, so tracing is wired up by naming alone.
  const client = new LanguageClient(
    "fai",
    "Fai Language Server",
    serverOptionsFor(folder),
    clientOptionsFor(folder),
  );
  clients.set(key, client);
  try {
    await client.start();
  } catch (error) {
    clients.delete(key);
    const message = error instanceof Error ? error.message : String(error);
    const openSettings = "Open Settings";
    const choice = await vscode.window.showErrorMessage(
      `Fai: failed to start the language server (${message}). Check the 'fai.server.path' setting.`,
      openSettings,
    );
    if (choice === openSettings) {
      await vscode.commands.executeCommand("workbench.action.openSettings", "fai.server.path");
    }
  }
}

async function stopClient(key: string): Promise<void> {
  const client = clients.get(key);
  if (!client) {
    return;
  }
  clients.delete(key);
  await client.stop();
}

async function restartAll(): Promise<void> {
  await Promise.all([...clients.keys()].map(stopClient));
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    await startClient(folder);
  }
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  outputChannel = vscode.window.createOutputChannel("Fai Language Server");
  context.subscriptions.push(outputChannel);

  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    await startClient(folder);
  }

  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders(async (event) => {
      for (const folder of event.removed) {
        await stopClient(folder.uri.toString());
      }
      for (const folder of event.added) {
        await startClient(folder);
      }
    }),
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (
        event.affectsConfiguration("fai.server.path") ||
        event.affectsConfiguration("fai.server.args") ||
        event.affectsConfiguration("fai.trace.server")
      ) {
        await restartAll();
      }
    }),
    vscode.commands.registerCommand("fai.restartServer", restartAll),
  );
}

export async function deactivate(): Promise<void> {
  await Promise.all([...clients.keys()].map(stopClient));
}
