import { createContext, memo, type ReactNode, useContext, useMemo } from "react";
import ReactMarkdown, { type Components, defaultUrlTransform } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import { barePaths, linkedPath, pathTarget } from "../lib/transcript";
import { openExternal } from "./openExternal";

/** Renders a path in the text (in a code span, a link or an image, or bare) as something that opens it. */
export type PathLinker = (path: string, children: ReactNode) => ReactNode;
export const PathLinks = createContext<PathLinker | null>(null);

/** Inside a link already: a code span in it stays plain. */
const InLink = createContext(false);

// `file://` links are paths too.
const urlTransform = (url: string) => (/^file:/i.test(url) ? url : defaultUrlTransform(url));

type MdNode = { type: string; value?: string; url?: string; children?: MdNode[] };

/** Bare paths in the text become links, like the ones a guy writes. (URLs already are, by GFM.) */
function remarkPaths() {
  const split = (value: string): MdNode[] => {
    const out: MdNode[] = [];
    let at = 0;
    for (const hit of barePaths(value)) {
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
  const linkPath = useContext(PathLinks);
  const components = useMemo<Components>(() => {
    const link: Components["a"] = ({ href, children }) => {
      const path = linkPath && href ? linkedPath(href) : null;
      const inner = <InLink.Provider value>{children}</InLink.Provider>;
      if (linkPath && path) return linkPath(path, inner);
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
    if (!linkPath) return { a: link };
    return {
      a: link,
      code: function Code({ className, children }) {
        const inLink = useContext(InLink);
        const code = <code className={className}>{children}</code>;
        // Inline code only: a block's has a language class or ends in a newline.
        const path = !inLink && !className && typeof children === "string" ? pathTarget(children) : null;
        return path ? linkPath(path, code) : code;
      },
      // A local image can't load as a plain <img>; it shows below the reply instead.
      img: ({ src, alt }) => {
        const path = typeof src === "string" && !/^(https?:|data:|blob:)/i.test(src) ? linkedPath(src) : null;
        if (!path) return <img src={src} alt={alt} />;
        return linkPath(path, alt || path.split("/").pop());
      },
    };
  }, [linkPath]);

  return (
    <div className="prose-chat selectable">
      <ReactMarkdown
        remarkPlugins={linkPath ? [remarkGfm, remarkPaths] : [remarkGfm]}
        rehypePlugins={[[rehypeHighlight, { detect: false, ignoreMissing: true }]]}
        urlTransform={urlTransform}
        components={components}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
