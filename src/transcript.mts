import type { MessageParam } from "@anthropic-ai/sdk/resources/messages.mjs";
import { DatabaseSync } from "node:sqlite";

const MAX_TRANSCRIPT = 500;
const WINDOW_STEP = 200;

export function initTranscript(db: DatabaseSync): void {
  db.exec(`CREATE TABLE IF NOT EXISTS transcript (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL
  )`);
}

export function transcriptPush(db: DatabaseSync, role: string, content: string): void {
  db.prepare("INSERT INTO transcript (ts, role, content) VALUES (?, ?, ?)").run(
    new Date().toISOString(), role, content
  );
}

export function transcriptLen(db: DatabaseSync): number {
  const row = db.prepare("SELECT COUNT(*) as n FROM transcript").get();
  return Number(row ? Object.values(row)[0] : 0);
}

export function transcriptLastTs(db: DatabaseSync): string {
  const row = db.prepare("SELECT ts FROM transcript ORDER BY id DESC LIMIT 1").get();
  return String(row ? Object.values(row)[0] : "");
}

export function buildMessages(db: DatabaseSync): MessageParam[] {
  const maxRow = db.prepare("SELECT COALESCE(MAX(id), 0) as id FROM transcript").get();
  const maxId = Number(maxRow ? Object.values(maxRow)[0] : 0);
  const desiredStart = Math.max(0, maxId - MAX_TRANSCRIPT);
  const anchorId = Math.floor(desiredStart / WINDOW_STEP) * WINDOW_STEP;

  const rows = db.prepare(
    "SELECT role, content FROM transcript WHERE id > ? ORDER BY id"
  ).all(anchorId);

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
