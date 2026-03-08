import type { MessageParam } from "@anthropic-ai/sdk/resources/messages.mjs";
import type { DatabaseSync } from "node:sqlite";

const MAX_TRANSCRIPT = 500;
const WINDOW_STEP = 200;

export interface Transcript {
  push(role: string, content: string): void;
  len(): number;
  lastTs(): string;
  buildMessages(): MessageParam[];
}

export function initTranscript(db: DatabaseSync): Transcript {
  db.exec(`CREATE TABLE IF NOT EXISTS transcript (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL
  )`);

  const insertRow = db.prepare(
    "INSERT INTO transcript (ts, role, content) VALUES (?, ?, ?)",
  );
  const countRows = db.prepare("SELECT COUNT(*) as n FROM transcript");
  const lastTimestamp = db.prepare(
    "SELECT ts FROM transcript ORDER BY id DESC LIMIT 1",
  );
  const maxRowId = db.prepare(
    "SELECT COALESCE(MAX(id), 0) as id FROM transcript",
  );
  const selectWindow = db.prepare(
    "SELECT role, content FROM transcript WHERE id > ? ORDER BY id",
  );

  return {
    push(role: string, content: string): void {
      insertRow.run(new Date().toISOString(), role, content);
    },

    len(): number {
      const row = countRows.get();
      return row ? Number(Object.values(row)[0]) : 0;
    },

    lastTs(): string {
      const row = lastTimestamp.get();
      return row ? String(Object.values(row)[0]) : "";
    },

    buildMessages(): MessageParam[] {
      const row = maxRowId.get();
      const maxId = row ? Number(Object.values(row)[0]) : 0;
      const desiredStart = Math.max(0, maxId - MAX_TRANSCRIPT);
      const anchorId = Math.floor(desiredStart / WINDOW_STEP) * WINDOW_STEP;

      const rows = selectWindow.all(anchorId);

      const messages: MessageParam[] = [];
      let currentRole: "user" | "assistant" | null = null;
      let currentParts: string[] = [];

      for (const row of rows) {
        const role: "user" | "assistant" =
          String(row.role) === "agent" ? "assistant" : "user";
        const text = String(row.content);

        if (role !== currentRole) {
          if (currentRole && currentParts.length > 0) {
            messages.push({
              role: currentRole,
              content: currentParts.join("\n\n"),
            });
          }
          currentRole = role;
          currentParts = [text];
        } else {
          currentParts.push(text);
        }
      }

      if (currentRole && currentParts.length > 0) {
        messages.push({
          role: currentRole,
          content: currentParts.join("\n\n"),
        });
      }

      if (messages.length === 0 || messages[0].role !== "user") {
        messages.unshift({ role: "user", content: "(repl started)" });
      }

      if (messages[messages.length - 1].role !== "user") {
        messages.push({ role: "user", content: "[heartbeat]" });
      }

      return messages;
    },
  };
}
