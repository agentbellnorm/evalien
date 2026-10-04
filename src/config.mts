import { readInferenceConfig, type InferenceConfig } from "./inference/config.mts";

/** The table holding the trajectory, for the runtime, the report, and the agent's own queries. */
export const TRAJECTORY_TABLE = "trajectory";

export interface StorageConfig {
  dbPath: string;
}

export interface Config extends StorageConfig {
  inference: InferenceConfig;
}

export function readStorageConfig(env: Record<string, string | undefined>): StorageConfig {
  return { dbPath: env.AGENT_DB_PATH || "/data/agent.db" };
}

/** Everything the agent reads from the environment. */
export function readConfig(env: Record<string, string | undefined>): Config {
  return { ...readStorageConfig(env), inference: readInferenceConfig(env) };
}
