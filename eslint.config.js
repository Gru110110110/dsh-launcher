import js from "@eslint/js";
import globals from "globals";
import hooks from "eslint-plugin-react-hooks";
import refresh from "eslint-plugin-react-refresh";
import boundaries from "eslint-plugin-boundaries";
import tseslint from "typescript-eslint";

export default tseslint.config(
  { ignores: ["dist", "src/platform/generated"] },
  js.configs.recommended,
  ...tseslint.configs.strictTypeChecked,
  {
    files: ["src/**/*.{ts,tsx}"],
    languageOptions: {
      ecmaVersion: 2022,
      globals: globals.browser,
      parserOptions: { projectService: true, tsconfigRootDir: import.meta.dirname },
    },
    plugins: { "react-hooks": hooks, "react-refresh": refresh, boundaries },
    settings: {
      "boundaries/elements": [
        { type: "app", pattern: "src/app/*" },
        { type: "feature", pattern: "src/features/*", capture: ["feature"] },
        { type: "platform", pattern: "src/platform/*" },
        { type: "shared", pattern: "src/shared/*" }
      ]
    },
    rules: {
      ...hooks.configs.recommended.rules,
      "react-refresh/only-export-components": ["warn", { "allowConstantExport": true }],
      "@typescript-eslint/no-floating-promises": "error",
      "@typescript-eslint/consistent-type-imports": "error",
      "boundaries/element-types": ["error", {
        default: "allow",
        rules: [
          { from: "shared", disallow: ["app", "feature", "platform"] },
          { from: "platform", disallow: ["app", "feature"] },
          { from: ["feature"], disallow: ["app"], allow: [["feature", { feature: "${from.feature}" }], "shared", "platform"] }
        ]
      }]
    }
  },
  {
    files: ["src/platform/launcherStore.ts", "src/platform/petStore.ts"],
    rules: { "@typescript-eslint/only-throw-error": "off" }
  }
);
