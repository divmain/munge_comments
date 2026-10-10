// Lazy-loaded routes.
const routes = [
  () => import(/* webpackChunkName: "home" */ "./home.js"),
  () => import(/* webpackChunkName: "about", webpackPrefetch: true */ "./about.js"),
  () => import(/* webpackIgnore: true */ "./external.js"),
  () => import(/* @vite-ignore */ "./dynamic.js"),
];

export const make = /*#__PURE__*/ createFactory();
export const other = /* @__PURE__ */ createFactory();

/*#__NO_SIDE_EFFECTS__*/
function createFactory() {
  return {};
}

export default routes;
