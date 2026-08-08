#!/usr/bin/env node
/**
 * Deterministic internal-link checker for the Raddy Starlight docs site.
 *
 * Scans every page under `src/content/docs/` and verifies that each internal
 * (site-relative) link resolves to an existing content page. External links,
 * same-page anchors, images, and links inside fenced code blocks are ignored.
 *
 * The custom 404 fallback page (`src/pages/404.astro`) is checked separately:
 * it is served at whatever URL the visitor landed on, so every internal link in
 * it MUST be an absolute site-root path carrying the `base` prefix
 * (`/raddy/...`) and must resolve to an existing page. Link targets are derived
 * from the file's own contents — there is no separately maintained list of
 * links to keep in sync.
 *
 * Links are resolved the way a browser would, against the page's own URL, so a
 * relative link such as `../config/sites/` written in `config/directives.md`
 * resolves to `/config/sites/` — matching how Starlight serves directory pages.
 *
 * Determinism: input files are walked in sorted order and findings are emitted
 * sorted by (file, line, column, target), so two runs always produce identical
 * output regardless of filesystem enumeration order. No network is used.
 *
 * Usage:
 *   node scripts/check-links.mjs
 *
 * Exits 0 when every link resolves, 1 otherwise.
 */
import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const PAGE_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const CONTENT_ROOT = join(PAGE_ROOT, 'src', 'content', 'docs');
// Custom 404 fallback page (see `disable404Route` in astro.config.mjs).
const CUSTOM_404 = join(PAGE_ROOT, 'src', 'pages', '404.astro');
// Site `base` from astro.config.mjs; a fully qualified internal link may carry
// it as a prefix (`/raddy/quickstart/`), which is stripped before resolution.
const SITE_BASE = '/raddy/';
// Matches the `base` as a path segment (`/raddy` followed by `/` or end), so
// stripping it turns `/raddy/quickstart/` into `/quickstart/` (and `/raddy/`
// into `/`). Keep `SITE_BASE` in sync with astro.config.mjs.
const BASE_PREFIX_RE = new RegExp(`^${SITE_BASE.replace(/\/$/, '')}(?=/|$)`);

const MARKDOWN_EXTS = ['.md', '.mdx'];
const PROTOCOL_RE = /^(?:[a-z][a-z0-9+.-]*:|\/\/)/i;

// Markdown links (`[text](url)`) and raw HTML `href` attributes. Both regexes
// are safe to reuse across files: `String.prototype.matchAll` clones the regex
// instead of advancing its `lastIndex`.
const linkRe = /\[[^\]]*\]\(([^)\s]+)(?:\s+["'][^"']*["'])?\)/g;
const hrefRe = /href\s*=\s*["']([^"']+)["']/g;

/** Recursively list content files, deterministically sorted. */
function walk(dir) {
	const entries = readdirSync(dir, { withFileTypes: true }).sort((a, b) =>
		a.name.localeCompare(b.name)
	);
	const files = [];
	for (const entry of entries) {
		if (entry.name.startsWith('_')) continue;
		const path = join(dir, entry.name);
		if (entry.isDirectory()) files.push(...walk(path));
		else if (MARKDOWN_EXTS.some((ext) => path.endsWith(ext))) files.push(path);
	}
	return files;
}

/** Build a set of content-relative paths (`config/directives.md`, `index.mdx`, …). */
function buildLookup(files) {
	return new Set(
		files.map((file) => relative(CONTENT_ROOT, file).split(sep).join('/'))
	);
}

/**
 * Return the URL (relative to the site root, including trailing slash) that a
 * content file is served at under Starlight's directory page format.
 * @param {string} relNoExt content-relative path with the extension removed
 */
function pageUrlFor(relNoExt) {
	if (relNoExt === 'index') return '/';
	if (relNoExt.endsWith('/index')) return `/${relNoExt.slice(0, -'/index'.length)}/`;
	return `/${relNoExt}/`;
}

/**
 * Map an absolute URL path (e.g. `/config/sites/`) to a content-relative file
 * path, or `null` when no page serves that path.
 * @param {string} pathname
 * @param {Set<string>} lookup
 */
function resolvePathToFile(pathname, lookup) {
	let p = pathname.replace(/^\/+/, '').replace(/\/+$/, '');
	if (p === '') p = 'index';
	if (p.endsWith('.html')) p = p.slice(0, -'.html'.length);
	for (const ext of MARKDOWN_EXTS) {
		if (p.endsWith(ext)) p = p.slice(0, -ext.length);
	}
	for (const candidate of [p, `${p}/index`]) {
		for (const ext of MARKDOWN_EXTS) {
			if (lookup.has(`${candidate}${ext}`)) return `${candidate}${ext}`;
		}
	}
	return null;
}

/** Replace matched spans with newlines so line numbers stay accurate. */
function blankOut(text, re) {
	return text.replace(re, (match) => match.replace(/[^\n]/g, ''));
}

/** Line and 1-based column for a match at `index` within `text`. */
function positionOf(text, index) {
	const before = text.slice(0, index);
	const line = before.split('\n').length;
	const column = index - before.lastIndexOf('\n');
	return { line, column };
}

/**
 * Scan a single file for internal links and return any that fail to resolve.
 * @param {object} opts
 * @param {string} opts.file absolute path of the file to scan
 * @param {string} opts.base base URL used to resolve relative links — the
 *   page's own URL for content files, or the site root for the 404 page (which
 *   is served at arbitrary missing URLs)
 * @param {Set<string>} opts.lookup content-relative path lookup
 * @param {boolean} [opts.requireBase] when true (404 page), every internal link
 *   must already carry the `SITE_BASE` prefix
 */
function scanLinks({ file, base, lookup, requireBase = false }) {
	const findings = [];
	// Preserve line numbers while dropping code blocks and HTML comments,
	// whose contents are author text, not navigable links.
	let text = blankOut(readFileSync(file, 'utf8'), /```[\s\S]*?```/g);
	text = blankOut(text, /<!--[\s\S]*?-->/g);

	const record = (match, url) => {
		// Skip images (`![alt](url)`) and any link preceded by `!`.
		if (match.index > 0 && text[match.index - 1] === '!') return;
		if (url === '' || url.startsWith('#') || url.startsWith('?') || PROTOCOL_RE.test(url)) {
			return;
		}
		const { line, column } = positionOf(text, match.index);
		// Links on the 404 page must carry the `base` prefix; otherwise they
		// resolve against the missing URL the page is served at and break (e.g.
		// `../` on `/raddy/missing` jumps to the domain root, not `/raddy/`).
		if (requireBase && !url.startsWith(SITE_BASE)) {
			findings.push({ file, line, column, url, target: '(relative link in 404 page)' });
			return;
		}
		let resolved;
		try {
			resolved = new URL(url, base);
		} catch {
			findings.push({ file, line, column, url, target: '(unparseable URL)' });
			return;
		}
		const target = resolved.pathname.replace(BASE_PREFIX_RE, '') || '/';
		const targetFile = resolvePathToFile(target, lookup);
		if (!targetFile) {
			findings.push({ file, line, column, url, target });
		}
	};

	for (const match of text.matchAll(linkRe)) record(match, match[1]);
	for (const match of text.matchAll(hrefRe)) record(match, match[1]);
	return findings;
}

const files = walk(CONTENT_ROOT);
const lookup = buildLookup(files);

const findings = [];

for (const file of files) {
	const relPath = relative(CONTENT_ROOT, file).split(sep).join('/');
	const relNoExt = relPath.replace(/\.(?:md|mdx)$/, '');
	const base = `https://docs.invalid${pageUrlFor(relNoExt)}`;
	findings.push(...scanLinks({ file, base, lookup }));
}

// The 404 page is served at the site root for any unmatched URL, so its links
// are resolved against the root and must already carry the `SITE_BASE` prefix.
findings.push(
	...scanLinks({ file: CUSTOM_404, base: 'https://docs.invalid/', lookup, requireBase: true })
);

findings.sort(
	(a, b) =>
		a.file.localeCompare(b.file) ||
		a.line - b.line ||
		a.column - b.column ||
		a.url.localeCompare(b.url)
);

for (const { file, line, column, url, target } of findings) {
	const where = relative(PAGE_ROOT, file) || file;
	console.error(`${where}:${line}:${column}: broken internal link \`${url}\` (resolves to \`${target}\`)`);
}

if (findings.length > 0) {
	console.error(`\ncheck-links: ${findings.length} broken internal link(s) found.`);
	process.exitCode = 1;
} else {
	console.log(`check-links: ${files.length + 1} pages checked, no broken internal links.`);
}
