import { readInferenceConfig, type InferenceConfig } from "./inference/config.mts";

export interface Config {
  dbPath: string;
  inference: InferenceConfig;
}

/** Everything read from the environment, once, before it's cleared. */
export function readConfig(env: Record<string, string | undefined>): Config {
  return {
    dbPath: env.AGENT_DB_PATH || "/data/agent.db",
    inference: readInferenceConfig(env),
  };
}
