// Expose only selected extension packages to application imports. Ordinary app
// imports retain their normal resolution; extension internals stay in the cache.
export function extensionImports(packages = {}) {
  return {
    name: "hibana-extension-imports",
    setup(builder) {
      builder.onResolve({ filter: /^[^./]/ }, async (args) => {
        if (args.pluginData?.hibanaExtension) return;
        const parts = args.path.split("/");
        const name = args.path.startsWith("@")
          ? parts.slice(0, 2).join("/")
          : parts[0];
        const root = Object.hasOwn(packages, name) ? packages[name] : undefined;
        if (!root) return;
        return builder.resolve(args.path, {
          resolveDir: root,
          kind: args.kind,
          pluginData: { hibanaExtension: true },
        });
      });
    },
  };
}
