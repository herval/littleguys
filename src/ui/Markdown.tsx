import { createContext, memo, type ReactNode, useContext, useMemo } from "react";
import ReactMarkdown, { type Components, defaultUrlTransform } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import { bareMedia, linkedMedia, type MediaRef, mediaTarget } from "../lib/transcript";
import { openExternal } from "./openExternal";

/** Renders a path in the text that points at media (in a code span, link or image) as something that opens it. */
export type MediaLinker = (media: MediaRef, children: ReactNode) => ReactNode;
export const MediaLinks = createContext<MediaLinker | null>(null);

/** Inside a link already: a code span in it stays plain. */
const InLink = createContext(false);

// `file://` links are paths too.
const urlTransform = (url: string) => (/^file:/i.test(url) ? url : defaultUrlTransform(url));

type MdNode = { type: string; value?: string; url?: string; children?: MdNode[] };

/** Bare paths to media in the text become links, like the ones a guy writes. (URLs already are, by GFM.) */
function remarkMediaPaths() {
  const split = (value: string): MdNode[] => {
    const out: MdNode[] = [];
    let at = 0;
    for (const hit of bareMedia(value).filter((h) => h.ref.path)) {
      if (hit.at > at) out.push({ type: "text", value: value.slice(at, hit.at) });
      out.push({ type: "link", url: hit.target, children: [{ type: "text", value: hit.target }] });
      at = hit.at + hit.target.length;
    }
    if (at < value.length) out.push({ type: "text", value: value.slice(at) });
    return out;
  };
  const walk = (node: MdNode) => {
    if (!node.children || node.type === "link" || node.type === "linkReference") return;
    node.children = node.children.flatMap((child) => {
      if (child.type === "text" && child.value) return split(child.value);
      walk(child);
      return [child];
    });
  };
  return walk;
}

export const Markdown = memo(function Markdown({ text }: { text: string }) {
  const linkMedia = useContext(MediaLinks);
  const components = useMemo<Components>(() => {
    const link: Components["a"] = ({ href, children }) => {
      const media = linkMedia && href ? linkedMedia(href) : null;
      const inner = <InLink.Provider value>{children}</InLink.Provider>;
      if (linkMedia && media?.path) return linkMedia(media, inner);
      return (
        <a
          href={href}
          onClick={(e) => {
            e.preventDefault();
            if (href) openExternal(href);
          }}
        >
          {inner}
        </a>
      );
    };
    if (!linkMedia) return { a: link };
    return {
      a: link,
      code: function Code({ className, children }) {
        const inLink = useContext(InLink);
        const code = <code className={className}>{children}</code>;
        // Inline code only: a block's has a language class or ends in a newline.
        const media = !inLink && !className && typeof children === "string" ? mediaTarget(children) : null;
        return media?.path ? linkMedia(media, code) : code;
      },
      // A local image can't load as a plain <img>; it shows below the reply instead.
      img: ({ src, alt }) => {
        const media = typeof src === "string" && !/^(https?:|data:|blob:)/i.test(src) ? linkedMedia(src) : null;
        if (!media?.path) return <img src={src} alt={alt} />;
        return linkMedia(media, alt || media.fileName);
      },
    };
  }, [linkMedia]);

  return (
    <div className="prose-chat selectable">
      <ReactMarkdown
        remarkPlugins={linkMedia ? [remarkGfm, remarkMediaPaths] : [remarkGfm]}
        rehypePlugins={[[rehypeHighlight, { detect: false, ignoreMissing: true }]]}
        urlTransform={urlTransform}
        components={components}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
