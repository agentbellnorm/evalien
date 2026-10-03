/** The runtime's complete contract with a source of model-generated code. */
export interface GenerateInput {
  instructions: string;
  blocks: readonly string[];
}

/** Return completed JavaScript source, or throw if generation failed. */
export type Generate = (input: GenerateInput) => Promise<string>;
