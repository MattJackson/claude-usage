// @ts-check
import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';

// usagio marketing site. Two pages only (/, /download/), no docs. Deployed
// to GitHub Pages from web/dist by .github/workflows/pages.yml.
export default defineConfig({
  site: 'https://usagio.app',
  trailingSlash: 'ignore',
  integrations: [sitemap()],
});
