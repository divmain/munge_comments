/// <reference types="node" />
/// <amd-module name="directives" />
/** @jest-environment node */
/** @jsx h */
/* eslint-disable no-console */
/* global window, document */
/* eslint eqeqeq: "off" */
// @ts-nocheck
// @flow

import { helper } from "./helper";

// @ts-ignore
const a: number = "not a number";
// @ts-expect-error
const b: string = 1;
// eslint-disable-next-line no-unused-vars
const unused = 1;
// tslint:disable-next-line:no-any
const loose: any = a;
// biome-ignore lint/style/useConst: reassigned below
let mutable = b;
// deno-lint-ignore no-explicit-any
const anything: any = mutable;
// prettier-ignore
const matrix = [1, 0,
                0, 1];
// $FlowFixMe
const flowy = matrix;

/* istanbul ignore next */
function uncovered(): number {
  return 0;
}

const lazy = import(/* webpackChunkName: "chunk" */ "./chunk");
const pure = /*#__PURE__*/ helper();

/** @license MIT */
/** @preserve */
const legal = 1;

// #region Helpers
/** @internal */
export function hidden(): void {}
// #endregion

const secret = "hunter2"; // NOSONAR
/* eslint-enable */
// vim: set ts=2 sw=2:
