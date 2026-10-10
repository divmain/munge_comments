// An ES module in TypeScript's .mts extension.
import { readFile } from "node:fs/promises";

/** Reads the configuration file. */
export async function load(path: string): Promise<string> {
  return readFile(path, "utf8"); // resolved relative to cwd
}
