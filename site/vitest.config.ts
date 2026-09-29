import { defineConfig } from 'vitest/config';

export default defineConfig({
  // `astro.config.mjs` defines this from Cargo.toml; tests only need it set.
  define: { __DEVSANDBOX_VERSION__: JSON.stringify('0.0.0-test') },
});
