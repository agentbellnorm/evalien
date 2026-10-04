// The report's composition root: read a stored run and check its quality.
import { readStorageConfig, TRAJECTORY_TABLE } from "./config.mts";
import { openDatabase } from "./db/open.mts";
import { createSqliteStore } from "./db/sqlite-store.mts";
import { parses } from "./evaluation/node-eval.mts";
import { renderTrajectory } from "./harness/context.mts";
import { analyze, lastRun, renderReport } from "./quality/analyze.mts";
import { createTrajectory } from "./trajectory/log.mts";

const args = process.argv.slice(2);
const flags = new Set(args.filter((arg) => arg.startsWith("--")));
const paths = args.filter((arg) => !arg.startsWith("--"));
if ([...flags].some((flag) => flag !== "--timeline" && flag !== "--last") || paths.length > 1) {
  process.stderr.write("usage: report.mts [db] [--timeline] [--last]\n");
  process.exit(64);
}

const db = openDatabase(paths[0] ?? readStorageConfig(process.env).dbPath, { readOnly: true });
const all = createTrajectory(createSqliteStore(db, TRAJECTORY_TABLE)).read();
db.close();

const entries = flags.has("--last") ? lastRun(all) : all;
const report = analyze(entries, { parses });
// The timeline is the trajectory as the model sees it, calls included.
if (flags.has("--timeline")) process.stdout.write(renderTrajectory(entries) + "\n");
process.stdout.write(renderReport(report));
process.exitCode = report.checks.every((check) => check.pass) ? 0 : 1;
