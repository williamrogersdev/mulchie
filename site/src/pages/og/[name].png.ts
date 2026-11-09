import type { APIRoute, GetStaticPaths } from 'astro';
import satori from 'satori';
import { Resvg } from '@resvg/resvg-js';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const root = process.cwd();
const font = (pkg: string, file: string) => readFileSync(resolve(root, 'node_modules', pkg, 'files', file));

const cards = {
  home: {
    title: 'Keep your machine usable while AI agents build on it.',
    sub: 'Monitor, optimize and reclaim. Open source.',
  },
  default: {
    title: 'Mulchie documentation',
    sub: 'The monitor and optimizer for machines that run many AI coding agents.',
  },
} as const;

export const getStaticPaths: GetStaticPaths = () => Object.keys(cards).map((name) => ({ params: { name } }));

type Node = { type: string; props: Record<string, unknown> };
const h = (type: string, style: Record<string, unknown>, children?: unknown, extra: Record<string, unknown> = {}): Node => ({
  type,
  props: { style: { display: 'flex', ...style }, children, ...extra },
});

export const GET: APIRoute = async ({ params }) => {
  const card = cards[params.name as keyof typeof cards];
  const icon = `data:image/png;base64,${readFileSync(resolve(root, 'public/icon-192.png')).toString('base64')}`;

  const tree = h(
    'div',
    { width: '100%', height: '100%', flexDirection: 'column', justifyContent: 'space-between', padding: 72, background: '#101410', color: '#e9eee4', fontFamily: 'Bricolage' },
    [
      h('div', { alignItems: 'center', gap: 20 }, [
        { type: 'img', props: { src: icon, width: 64, height: 64 } },
        h('div', { fontSize: 38, fontWeight: 700 }, 'Mulchie'),
      ]),
      h('div', { flexDirection: 'column', gap: 28 }, [
        h('div', { fontSize: 76, fontWeight: 700, lineHeight: 1.05, letterSpacing: -2, maxWidth: 980 }, card.title),
        h('div', { fontSize: 32, color: '#a5b0a1', maxWidth: 900 }, card.sub),
      ]),
      h('div', { fontSize: 30, color: '#8fd17e', fontWeight: 700 }, 'mulchie.com'),
    ],
  );

  const svg = await satori(tree as never, {
    width: 1200,
    height: 630,
    fonts: [
      { name: 'Bricolage', data: font('@fontsource/bricolage-grotesque', 'bricolage-grotesque-latin-700-normal.woff'), weight: 700, style: 'normal' },
      { name: 'Bricolage', data: font('@fontsource/bricolage-grotesque', 'bricolage-grotesque-latin-400-normal.woff'), weight: 400, style: 'normal' },
    ],
  });
  const png = new Resvg(svg, { fitTo: { mode: 'width', value: 1200 } }).render().asPng();
  return new Response(new Uint8Array(png), { headers: { 'Content-Type': 'image/png' } });
};
