/** @type {import('@stryker-mutator/core').PartialStrykerOptions} */
export default {
  // Stay on the `command` runner. The dedicated Bun runner's parser is
  // not yet aligned with Bun 1.3.x output (no per-test lines on macOS,
  // "X pass" / "Y fail" on separate lines), so it under-reports. Plain
  // `bun test` is fast enough since dryRun is small.
  testRunner: "command",
  commandRunner: { command: "bun test" },
  coverageAnalysis: "off",
  disableTypeChecks: "**/*.{ts,tsx,js,mjs}",
  mutate: ["src/**/*.ts", "!src/**/*.d.ts"],
  reporters: ["html", "clear-text", "progress", "json"],
  htmlReporter: { fileName: "reports/stryker/index.html" },
  jsonReporter: { fileName: "reports/stryker/mutation-report.json" },
  concurrency: 4,
  timeoutMS: 20000,
  dryRunTimeoutMinutes: 10,
  incremental: true,
  incrementalFile: "reports/stryker-incremental.json",
  mutator: {
    excludedMutations: [
      "StringLiteral",
      "ObjectLiteral",
      "BlockStatement",
      "Regex",
      "ArrayDeclaration",
    ],
  },
};
