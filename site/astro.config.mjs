// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import tailwindcss from '@tailwindcss/vite';

// Served by GitHub Pages from the manby repository; with a domain of its
// own, `site` becomes that domain and `base` goes.
const site = 'https://birkskyum.github.io';
const base = '/manby';

// The social card, made by og/render.sh from og/card.html. Crawlers need
// its address in full.
const card = [
	{ tag: 'meta', attrs: { property: 'og:image', content: `${site}${base}/og.png` } },
	{ tag: 'meta', attrs: { property: 'og:image:width', content: '2400' } },
	{ tag: 'meta', attrs: { property: 'og:image:height', content: '1260' } },
	{
		tag: 'meta',
		attrs: {
			property: 'og:image:alt',
			content: 'manby, 8 to 18 times faster than tippecanoe. Same options, same tiles, less memory. Tiling 4 million points takes tippecanoe 61.4 seconds and manby 3.44.',
		},
	},
	{ tag: 'meta', attrs: { name: 'twitter:image', content: `${site}${base}/og.png` } },
];

export default defineConfig({
	site,
	base,
	integrations: [
		starlight({
			title: 'manby',
			description:
				'Builds vector tilesets from geographic features. The same tiles as tippecanoe, eight to eighteen times faster, in less memory.',
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/birkskyum/manby' }],
			head: card,
			// global.css first: its @layer order has to come before any other
			// stylesheet names a layer, or Tailwind's base resets win.
			customCss: ['./src/styles/global.css', '@fontsource-variable/inter'],
			components: { SiteTitle: './src/components/SiteTitle.astro' },
			expressiveCode: {
				themes: ['github-dark', 'github-light'],
				styleOverrides: { borderRadius: '0.75rem' },
			},
			sidebar: [
				{ label: 'Why manby?', slug: 'why' },
				{ label: 'Getting started', slug: 'getting-started' },
				{
					label: 'Guides',
					items: [
						{ label: 'Inputs larger than memory', slug: 'guides/memory' },
						{ label: 'Joining tilesets', slug: 'guides/join' },
						{ label: 'Fitting each tile on its own', slug: 'guides/per-tile' },
						{ label: "tippecanoe's other programs", slug: 'guides/programs' },
						{ label: 'MapLibre Tiles', slug: 'guides/mlt' },
						{ label: 'GeoParquet, DuckDB and Python', slug: 'guides/python' },
						{ label: 'From R', slug: 'guides/r' },
						{ label: 'Looking at tilesets', slug: 'guides/serve' },
						{ label: 'H3 hexagons and categories', slug: 'guides/h3' },
						{ label: 'Other formats', slug: 'guides/other-formats' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'Benchmarks', slug: 'reference/benchmarks' },
						{ label: 'How it works', slug: 'reference/how' },
						{ label: 'Differences from tippecanoe', slug: 'reference/differences' },
						{ label: 'Changelog', slug: 'reference/changelog' },
					],
				},
			],
		}),
	],
	vite: {
		plugins: [tailwindcss()],
		// the changelog page shows the repository's CHANGELOG.md
		server: { fs: { allow: ['..'] } },
	},
});
