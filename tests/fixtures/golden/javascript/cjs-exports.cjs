/** Exported via module.exports.alpha: doc-protected. */
function alpha() {}
module.exports.alpha = alpha;

/** Exported via exports.beta: doc-protected. */
function beta() {}
exports.beta = beta;

/** Exported as module.exports itself: doc-protected. */
function gamma() {}

/** Exported via shorthand in an object literal: doc-protected. */
function delta() {}

/** Exported via a key-value pair: doc-protected. */
function epsilon() {}

/** Not exported: prose. */
function internal() {}

/** JSDoc on the export assignment itself: doc-protected. */
module.exports.zeta = function zeta() {};

/** JSDoc on an exports.name assignment: doc-protected. */
exports.eta = () => internal();

module.exports = gamma;
module.exports = { delta, epsilon: epsilon };
