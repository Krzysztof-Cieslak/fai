import * as vscode from "vscode";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
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
const packageStatus = new Map<string, vscode.StatusBarItem>();
const compilerSelections = new Map<string, string | undefined>();
const selectionTimers = new Map<string, ReturnType<typeof setTimeout>>();
const selectionChecks = new Map<string, Promise<void>>();
const executeFile = promisify(execFile);
let deactivating = false;

async function compilerSelection(folder: vscode.WorkspaceFolder): Promise<string | undefined> {
  const command = vscode.workspace.getConfiguration("fai", folder.uri).get<string>("server.path", "fai");
  try {
    const { stdout } = await executeFile(command, ["toolchain", "which", "--message-format=json", "--project", folder.uri.fsPath],
      { cwd: folder.uri.fsPath, timeout: 15000, maxBuffer: 1024 * 1024 });
    const result = JSON.parse(stdout);
    const compiler = result.result?.compiler;
    if (result.schemaVersion === 1 && compiler) {
      return JSON.stringify([compiler.version, compiler.executable, compiler.buildId]);
    }
  } catch {
    // Direct development compilers may not implement the launcher's selection query.
  }
  return undefined;
}

function scheduleSelectionCheck(folder: vscode.WorkspaceFolder): void {
  const key = folder.uri.toString();
  clearTimeout(selectionTimers.get(key));
  selectionTimers.set(key, setTimeout(() => {
    selectionTimers.delete(key);
    const previous = selectionChecks.get(key) ?? Promise.resolve();
    const next = previous.catch(() => {}).then(async () => {
      if (deactivating || !vscode.workspace.workspaceFolders?.some(item => item.uri.toString() === key)) return;
      const selection = await compilerSelection(folder);
      if (deactivating) return;
      if (selection === undefined || selection !== compilerSelections.get(key)) {
        await stopClient(key);
        await startClient(folder);
      }
    }).catch(error => outputChannel?.appendLine(`Compiler selection: ${String(error)}`));
    selectionChecks.set(key, next);
  }, 300));
}

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
    documentSelector: [
      { scheme: "file", language: "fai", pattern: glob },
      { scheme: "file", pattern: `${folder.uri.fsPath.replace(/\\/g, "/")}/**/fai.toml` },
      { scheme: "file", pattern: `${folder.uri.fsPath.replace(/\\/g, "/")}/**/fai.local.toml` },
    ],
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
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 10);
  packageStatus.set(key, status);
  status.command = "fai.showOutput";
  client.onNotification("$/fai/projectStatus", (event: { schemaVersion: number; state: string }) => {
    if (event.schemaVersion !== 1) return;
    if (event.state === "ready") {
      status.hide();
    } else if (event.state === "restoring") {
      status.text = "$(sync~spin) Fai dependencies";
      status.tooltip = `Restoring ${folder.name} package dependencies`;
      status.show();
    } else if (event.state === "failed") {
      status.text = "$(warning) Fai dependencies";
      status.tooltip = `Dependency restoration failed for ${folder.name}; open Fai output`;
      status.show();
    }
  });
  try {
    const selection = await compilerSelection(folder);
    if (deactivating || clients.get(key) !== client) return;
    compilerSelections.set(key, selection);
    await client.start();
  } catch (error) {
    clients.delete(key);
    packageStatus.delete(key);
    status.dispose();
    const message = error instanceof Error ? error.message : String(error);
    const openSettings = "Open Settings";
    const choice = await vscode.window.showErrorMessage(
      `Fai: failed to start the language server (${message}). Run 'fai toolchain which' in this project to check its compiler, or check 'fai.server.path'.`,
      openSettings,
    );
    if (choice === openSettings) {
      await vscode.commands.executeCommand("workbench.action.openSettings", "fai.server.path");
    }
  }
}

async function stopClient(key: string): Promise<void> {
  clearTimeout(selectionTimers.get(key));
  selectionTimers.delete(key);
  compilerSelections.delete(key);
  const client = clients.get(key);
  if (!client) {
    return;
  }
  clients.delete(key);
  packageStatus.get(key)?.dispose();
  packageStatus.delete(key);
  await client.stop();
}

async function restartAll(): Promise<void> {
  await Promise.all([...clients.keys()].map(stopClient));
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    await startClient(folder);
  }
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  deactivating = false;
  outputChannel = vscode.window.createOutputChannel("Fai Language Server");
  context.subscriptions.push(outputChannel);
  const manifests = vscode.workspace.createFileSystemWatcher("**/fai.toml");
  const changed = () => {
    for (const folder of vscode.workspace.workspaceFolders ?? []) scheduleSelectionCheck(folder);
  };
  context.subscriptions.push(manifests, manifests.onDidChange(changed), manifests.onDidCreate(changed), manifests.onDidDelete(changed));

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
    vscode.commands.registerCommand("fai.showOutput", () => outputChannel?.show()),
  );
}

export async function deactivate(): Promise<void> {
  deactivating = true;
  for (const timer of selectionTimers.values()) clearTimeout(timer);
  selectionTimers.clear();
  await Promise.all(selectionChecks.values());
  selectionChecks.clear();
  await Promise.all([...clients.keys()].map(stopClient));
}
