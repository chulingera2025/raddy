// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// Lightweight TextMate grammar for the Raddexfile (Caddyfile-like DSL), so
// `caddyfile` code blocks get real syntax highlighting instead of a txt
// fallback. Covers comments, strings, numbers, directive keywords,
// `{placeholder}` tokens, and block braces.
const caddyfileLang = {
	name: 'caddyfile',
	scopeName: 'source.caddyfile',
	patterns: [
		{ include: '#comment' },
		{ include: '#string' },
		{ include: '#number' },
		{ include: '#placeholder' },
		{ include: '#directive' },
		{ include: '#braces' },
	],
	repository: {
		comment: { match: /#.*$/, name: 'comment.line.number-sign.caddyfile' },
		string: { begin: /"/, end: /"/, name: 'string.quoted.double.caddyfile' },
		number: { match: /\b\d+\b/, name: 'constant.numeric.caddyfile' },
		placeholder: {
			match: /\{[a-z_]+}/,
			name: 'constant.other.placeholder.caddyfile',
		},
		directive: {
			match: /\b(?:reverse_proxy|file_server|redir|handle|handle_path|rewrite|respond|error|root|encode|header_up|header_down|rate_limit|basic_auth|forward_auth|trusted_proxies|tls|access_log|import|snippet|lb_policy|health_check|to|interval|timeout|consecutive_failures|consecutive_successes|log_level|acme_email|dns_challenge|min_version|max_version|ciphers|client_auth|tls_servername|tls_skip_verify|tls_ca|tls_cert|admin)\b/,
			name: 'keyword.control.caddyfile',
		},
		braces: { match: /[{}]/, name: 'punctuation.section.block.caddyfile' },
	},
};

// https://astro.build/config
export default defineConfig({
	site: 'https://chulingera2025.github.io',
	base: '/raddex/',
	integrations: [
		starlight({
			title: 'Raddex',
			description: 'A minimal high-performance reverse proxy gateway built on Cloudflare Pingora.',
			// English is the default (root) locale; Simplified Chinese lives
			// under /zh-CN/. Locale directories:
			//   src/content/docs/          → English (root)
			//   src/content/docs/zh-cn/    → 简体中文
			defaultLocale: 'root',
			locales: {
				root: { label: 'English', lang: 'en' },
				'zh-cn': { label: '简体中文', lang: 'zh-CN' },
			},
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/chulingera2025/raddex' },
			],
			editLink: {
				baseUrl: 'https://github.com/chulingera2025/raddex/edit/main/page',
			},
			// The 404 fallback is a fully custom page (`src/pages/404.astro`) so the
			// injected Starlight 404 route is disabled to avoid the `/404` route
			// conflict that otherwise breaks builds of the `docs` content collection.
			disable404Route: true,
			// Explicit crawl directives for every page, alongside public/robots.txt.
			// Starlight already emits canonical/OG/Twitter metadata for each page.
			// The custom 404 page overrides this per page (`noindex, follow`) via its
			// frontmatter `head`, which Starlight merges by replacing the site-wide tag.
			head: [
				{ tag: 'meta', attrs: { name: 'robots', content: 'index, follow' } },
			],
			expressiveCode: {
				shiki: { langs: [caddyfileLang] },
			},
			sidebar: [
				{
					label: 'Getting Started',
					translations: { 'zh-CN': '快速开始' },
					items: [
						{ label: 'Quick start', translations: { 'zh-CN': '快速上手' }, slug: 'quickstart' },
						{ label: 'Installation', translations: { 'zh-CN': '安装' }, slug: 'install' },
					],
				},
				{
					label: 'Guides',
					translations: { 'zh-CN': '指南' },
					items: [
						{ label: 'HTTPS & TLS', translations: { 'zh-CN': 'HTTPS 与 TLS' }, slug: 'guides/https-tls' },
						{ label: 'Routing & matchers', translations: { 'zh-CN': '路由与匹配器' }, slug: 'guides/routing' },
						{ label: 'Authentication', translations: { 'zh-CN': '认证' }, slug: 'guides/auth' },
						{ label: 'Compression', translations: { 'zh-CN': '压缩' }, slug: 'guides/compression' },
						{ label: 'Configuration reuse', translations: { 'zh-CN': '配置复用' }, slug: 'guides/config-dx' },
						{ label: 'Serve static files', translations: { 'zh-CN': '静态托管' }, slug: 'guides/static-files' },
						{ label: 'Redirect HTTP → HTTPS', translations: { 'zh-CN': 'HTTP → HTTPS 重定向' }, slug: 'guides/http-to-https' },
						{ label: 'Proxy an API', translations: { 'zh-CN': '代理 API' }, slug: 'guides/api-proxy' },
						{ label: 'Layer 4 (TCP & UDP)', translations: { 'zh-CN': '四层代理（TCP 与 UDP）' }, slug: 'guides/layer4' },
						{ label: 'Migrate from Caddy or nginx', slug: 'guides/migration' },
					],
				},
				{
					label: 'Architecture',
					translations: { 'zh-CN': '架构' },
					items: [
						{ label: 'Capability matrix', translations: { 'zh-CN': '能力矩阵' }, slug: 'architecture/capabilities' },
					],
				},
				{
					label: 'Raddexfile',
					translations: { 'zh-CN': 'Raddexfile' },
					items: [
						{ label: 'Concepts', translations: { 'zh-CN': '核心概念' }, slug: 'config' },
						{ label: 'Directives', translations: { 'zh-CN': '指令参考' }, slug: 'config/directives' },
						{ label: 'Sites, ports & HTTPS', translations: { 'zh-CN': '站点 · 端口 · HTTPS' }, slug: 'config/sites' },
						{ label: 'Trusted proxies', translations: { 'zh-CN': '可信代理' }, slug: 'config/trusted-proxies' },
					],
				},
				{
					label: 'CLI & Operations',
					translations: { 'zh-CN': 'CLI 与运维' },
					items: [
						{ label: 'CLI reference', translations: { 'zh-CN': 'CLI 参考' }, slug: 'cli' },
						{ label: 'Deployment and operations', slug: 'operations/deployment' },
						{ label: 'Troubleshooting', slug: 'operations/troubleshooting' },
						{ label: 'Metrics', translations: { 'zh-CN': '指标' }, slug: 'operations/metrics' },
						{ label: 'Access log', translations: { 'zh-CN': '访问日志' }, slug: 'operations/access-log' },
						{ label: 'Performance', translations: { 'zh-CN': '性能' }, slug: 'performance' },
					],
				},
			],
		}),
	],
});
