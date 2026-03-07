import type { MessageParam } from "@anthropic-ai/sdk/resources/messages.mjs";
import type { DatabaseSync, StatementSync } from "node:sqlite";

const MAX_TRANSCRIPT = 500;
const WINDOW_STEP = 200;

let insertRow: StatementSync;
let countRows: StatementSync;
let lastTimestamp: StatementSync;
let maxRowId: StatementSync;
let selectWindow: StatementSync;

function firstVal(row: Record<string, unknown> | undefined, fallback: string | number): string | number {
  if (!row) return fallback;
  const vals = Object.values(row);
  return vals.length > 0 ? (vals[0] as string | number) : fallback;
}

export function initTranscript(db: DatabaseSync): void {
  db.exec(`CREATE TABLE IF NOT EXISTS transcript (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL
  )`);
  insertRow = db.prepare("INSERT INTO transcript (ts, role, content) VALUES (?, ?, ?)");
  countRows = db.prepare("SELECT COUNT(*) as n FROM transcript");
  lastTimestamp = db.prepare("SELECT ts FROM transcript ORDER BY id DESC LIMIT 1");
  maxRowId = db.prepare("SELECT COALESCE(MAX(id), 0) as id FROM transcript");
  selectWindow = db.prepare("SELECT role, content FROM transcript WHERE id > ? ORDER BY id");
}

export function transcriptPush(role: string, content: string): void {
  insertRow.run(new Date().toISOString(), role, content);
}

export function transcriptLen(): number {
  return Number(firstVal(countRows.get(), 0));
}

export function transcriptLastTs(): string {
  return String(firstVal(lastTimestamp.get(), ""));
}

export function buildMessages(): MessageParam[] {
  const maxId = Number(firstVal(maxRowId.get(), 0));
  const desiredStart = Math.max(0, maxId - MAX_TRANSCRIPT);
  const anchorId = Math.floor(desiredStart / WINDOW_STEP) * WINDOW_STEP;

  const rows = selectWindow.all(anchorId);

  const messages: MessageParam[] = [];
  let currentRole: "user" | "assistant" | null = null;
  let currentParts: string[] = [];

  for (const row of rows) {
    const role: "user" | "assistant" = String(row.role) === "agent" ? "assistant" : "user";
    const text = String(row.content);

    if (role !== currentRole) {
      if (currentRole && currentParts.length > 0) {
        messages.push({ role: currentRole, content: currentParts.join("\n\n") });
      }
      currentRole = role;
      currentParts = [text];
    } else {
      currentParts.push(text);
    }
  }

  if (currentRole && currentParts.length > 0) {
    messages.push({ role: currentRole, content: currentParts.join("\n\n") });
  }

  if (messages.length === 0 || messages[0].role !== "user") {
    messages.unshift({ role: "user", content: "(repl started)" });
  }

  if (messages[messages.length - 1].role !== "user") {
    messages.push({ role: "user", content: "[heartbeat]" });
  }

  return messages;
}
