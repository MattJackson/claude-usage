// @ts-check
import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';

// usagio marketing site. Two pages only (/, /download/), no docs. Deployed
// to GitHub Pages from web/dist by .github/workflows/pages.yml.
// While hosted at mattjackson.github.io/claude-usage/, `base` must match the
// sub-path so asset URLs (/_astro/*, /icons/*, etc.) resolve. Remove `base`
// (or set to '/') once we cut over to the usagio.app custom domain at the
// apex. Then Astro will emit root-relative URLs again.
export default defineConfig({
  site: 'https://usagio.app',
  base: '/claude-usage/',
  trailingSlash: 'ignore',
  integrations: [sitemap()],
});
