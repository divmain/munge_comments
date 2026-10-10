/** ESM export: doc-protected. */
export function alpha() {}

/** CommonJS shapes do not export in .mjs: prose. */
function beta() {}
exports.beta = beta;

/** Not exported: prose. */
function internal() {}
