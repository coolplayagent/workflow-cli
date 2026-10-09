"""CommonMark links and heading IDs shared by the book and skill validators."""
import os
from itertools import product
import re
from urllib.parse import quote, unquote, urlsplit

from markdown_it import MarkdownIt


def parse(text):
    frontmatter = re.match(r'\A---\r?\n.*?\r?\n---[ \t]*(?:\r?\n|$)', text, re.S)
    if frontmatter:
        # Preserve source line numbers for the package rewriter's code exclusions.
        text = '\n' * frontmatter[0].count('\n') + text[frontmatter.end():]
    md = MarkdownIt('commonmark', {'html': False}).enable('table')
    tokens = md.parse(text)
    used, anchors = set(), set()
    for index, token in enumerate(tokens):
        if token.type != 'heading_open':
            continue
        inline = tokens[index + 1]
        label = ''.join(child.content for child in inline.children or []
                        if child.type in ('text', 'code_inline', 'image'))
        slug = re.sub(r'[^\w\- ]', '', label.lower()).replace(' ', '-')
        candidate, counter = slug, 0
        while candidate in used:
            counter += 1
            candidate = f'{slug}-{counter}'
        used.add(candidate)
        anchors.add(candidate)
        token.attrSet('id', candidate)
    return md, tokens, anchors


def link_tokens(tokens):
    for token in tokens:
        if token.type in ('link_open', 'image'):
            yield token, 'src' if token.type == 'image' else 'href'
        yield from link_tokens(token.children or [])


def local_target(source, href, root):
    url = urlsplit(href)
    if url.scheme or url.netloc:
        if url.scheme not in ('http', 'https', 'mailto') and not url.netloc:
            raise ValueError(f'Unsupported link scheme: {source}: {href}')
        return None
    path = unquote(url.path)
    if path.startswith('/') or '\\' in path:
        raise ValueError(f'Nonportable local link: {source}: {href}')
    target = (source.parent / path).resolve() if path else source.resolve()
    if not target.is_relative_to(root.resolve()) or not target.exists():
        raise ValueError(f'Missing or escaping local link: {source}: {href}')
    return target, unquote(url.fragment)


def check_links(paths, root):
    documents, anchors, count = {}, {}, 0
    for source in paths:
        _, tokens, ids = parse(source.read_text())
        documents[source] = tokens
        anchors[source.resolve()] = ids
    for source, tokens in documents.items():
        for token, attribute in link_tokens(tokens):
            href = token.attrGet(attribute)
            resolved = local_target(source, href, root)
            if resolved is None:
                continue
            target, fragment = resolved
            if fragment and target.suffix == '.md':
                if target not in anchors:
                    anchors[target] = parse(target.read_text())[2]
                if fragment not in anchors[target]:
                    raise ValueError(f'Missing anchor: {source}: {href}')
            count += 1
    return count


def rewrite_links(text, source, destination, mapping, root):
    """Map parsed destinations while retaining authored Markdown, including titles."""
    destinations = {token.attrGet(attr) for token, attr in link_tokens(parse(text)[1])}
    replacements = {}
    for href in destinations:
        resolved = local_target(source, href, root)
        if resolved is None:
            continue
        target, _ = resolved
        if target not in mapping:
            raise ValueError(f'Linked resource is absent from package: {source}: {href}')
        url = urlsplit(href)
        rewritten = quote(os.path.relpath(mapping[target], destination.parent), safe='/.-_')
        if url.query:
            rewritten += '?' + url.query
        if url.fragment:
            rewritten += '#' + unquote(url.fragment)
        for path, query, fragment in product((url.path, unquote(url.path)),
                                              (url.query, unquote(url.query)),
                                              (url.fragment, unquote(url.fragment))):
            authored = path + ('?' + query if query else '') + ('#' + fragment if fragment else '')
            replacements[authored] = rewritten
    if not replacements:
        return text
    _, tokens, _ = parse(text)
    lines = text.splitlines(keepends=True)
    offsets = [0]
    for line in lines:
        offsets.append(offsets[-1] + len(line))
    # Code examples are not links and must retain their literal bytes.
    excluded = [(offsets[t.map[0]], offsets[t.map[1]]) for t in tokens
                if t.type in ('fence', 'code_block') and t.map]
    excluded += [m.span() for m in re.finditer(r'(`+)(?!`)(.+?)(?<!`)\1(?!`)', text, re.S)]
    alternatives = '|'.join(re.escape(href) for href in sorted(replacements, key=len, reverse=True))
    pattern = rf'(\]\(\s*<?|^ {{0,3}}\[[^\]\n]+\]:\s*<?)({alternatives})(?=>|\s|\))'
    seen = set()

    def replace(match):
        if any(start <= match.start() < end for start, end in excluded):
            return match[0]
        seen.add(replacements[match[2]])
        return match[1] + replacements[match[2]]

    result = re.sub(pattern, replace, text, flags=re.M)
    if seen != set(replacements.values()):
        raise ValueError(f'Cannot rewrite every parsed destination: {source}')
    return result
