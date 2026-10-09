"""Regressions for broken reading paths, offline packaging and rendered links."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from book import load_book, sync
from build_site import build, check_site
from doc_links import check_links, parse, rewrite_links
from package_skill import copy_resources
from sync_skill_resources import sync_resources


class DocumentLinks(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='book tests ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / 'guide.md'
        self.target = self.root / 'part (one).md'
        self.target.write_text('# **Getting** `ready`\n\n## 重复标题\n\n## 重复标题\n')

    def test_commonmark_links_images_reference_definitions_and_unicode_fragments(self):
        self.source.write_text('[chapter](<part (one).md#getting-ready> "Read chapter")\n'
                               '[again][target]\n\n[target]: part%20(one).md#重复标题-1\n'
                               '![figure](diagram.svg)\n')
        (self.root / 'diagram.svg').write_text('<svg/>')
        self.assertEqual(check_links([self.source, self.target], self.root), 3)

    def test_missing_file_and_self_or_cross_page_anchors_fail(self):
        for href in ['missing.md', '#missing', 'part%20(one).md#missing']:
            with self.subTest(href=href):
                self.source.write_text(f'# Existing\n\n[bad]({href})\n')
                with self.assertRaises(ValueError):
                    check_links([self.source], self.root)

    def test_escaping_and_symlink_destinations_fail(self):
        (self.root / 'escape').symlink_to('/etc')
        for href in ['../missing', '/etc/passwd', 'escape/passwd', '%2E%2E/outside']:
            with self.subTest(href=href):
                self.source.write_text(f'[bad]({href})\n')
                with self.assertRaises(ValueError):
                    check_links([self.source], self.root)

    def test_fenced_and_inline_code_are_not_interpreted_as_links(self):
        self.source.write_text('```md\n[not a link](missing.md)\n```\n\n'
                               '`[literal](also-missing.md)`\n')
        self.assertEqual(check_links([self.source], self.root), 0)

    def test_duplicate_and_formatted_headings_have_unique_rendered_ids(self):
        anchors = parse('# A\n\n## A\n\n## A-1\n\n## **A**\n')[2]
        self.assertEqual(anchors, {'a', 'a-1', 'a-1-1', 'a-2'})

    def test_rewrite_preserves_titles_code_and_balanced_parentheses(self):
        text = ('[chapter](<part (one).md#getting-ready> "Read chapter")\n'
                '[again][target]\n\n[target]: part%20(one).md#重复标题-1\n\n'
                '`[literal](part%20(one).md#重复标题-1)`\n\n'
                '```md\n[not a link](part%20(one).md#重复标题-1)\n```\n')
        output = self.root / 'package'
        output.mkdir()
        target = output / 'chapter.md'
        target.write_text(self.target.read_text())
        rewritten = rewrite_links(text, self.source, output / 'index.md',
                                  {self.target: target}, self.root)
        self.assertIn('[chapter](<chapter.md#getting-ready> "Read chapter")', rewritten)
        self.assertIn('[target]: chapter.md#重复标题-1', rewritten)
        self.assertIn('`[literal](part%20(one).md#重复标题-1)`', rewritten)
        self.assertIn('```md\n[not a link](part%20(one).md#重复标题-1)\n```', rewritten)
        (output / 'index.md').write_text(rewritten)
        self.assertEqual(check_links(list(output.glob('*.md')), output), 2)

    def test_missing_packaged_resource_cannot_fall_back_to_github(self):
        with self.assertRaisesRegex(ValueError, 'absent from package'):
            rewrite_links('[read](<part (one).md>)', self.source,
                          self.root / 'package/index.md', {}, self.root)

    def test_skill_frontmatter_preserves_literal_code_during_rewrite(self):
        content = ('---\nname: example\ndescription: A --- separator in metadata\n'
                   'metadata:\n  version: "1"\n---\n\n'
                   '```md\n[literal](<part (one).md#getting-ready>)\n```\n\n'
                   '[read](<part (one).md#getting-ready>)\n')
        result = rewrite_links(content, self.source, self.root / 'package/index.md',
                               {self.target: self.root / 'package/chapter.md'}, self.root)
        self.assertIn('```md\n[literal](<part (one).md#getting-ready>)\n```', result)
        self.assertIn('[read](<chapter.md#getting-ready>)', result)
        self.assertEqual(parse(content)[2], set())


class Distribution(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='distribution tests ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'source'
        self.root.mkdir()

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def test_transitive_reading_closure_survives_source_removal(self):
        skill = 'skills/workflow-cli/'
        self.write(skill + 'SKILL.md', '# Skill\n\n[route](references/task.md)')
        self.write(skill + 'references/task.md', '# Task\n\n[manual](manuals/en/01-start/01-guide.md#run)')
        self.write(skill + 'references/manuals/en/01-start/01-guide.md',
                   '# Guide\n\n## Run\n\n[中文](../../zh/01-start/01-guide.md)\n'
                   '[example](../../../../assets/examples/input.json)\n'
                   '[schema](../../../../assets/schemas/input.json)\n[skill](../../../../SKILL.md)')
        self.write(skill + 'references/manuals/zh/01-start/01-guide.md',
                   '# 中文\n\n[English](../../en/01-start/01-guide.md#run)')
        self.write(skill + 'assets/examples/input.json', '{}')
        self.write(skill + 'assets/schemas/input.json', '{}')
        # No repository docs/examples/schemas exist. Packaging reads only the skill.
        package = self.root.parent / 'installed skill'
        copy_resources(self.root, package)
        self.root.rename(self.root.parent / 'unavailable-source')
        self.assertEqual(check_links(list(package.rglob('*.md')), package), 7)
        self.assertIn('manuals/en/01-start/01-guide.md#run', (package / 'references/task.md').read_text())
        self.assertTrue((package / 'assets/examples/input.json').is_file())
        (package / 'references/manuals/zh/01-start/01-guide.md').unlink()
        with self.assertRaises(ValueError):
            check_links(list(package.rglob('*.md')), package)

    def test_packaging_rejects_links_to_existing_repository_docs(self):
        self.write('docs/guide.md', '# Guide')
        self.write('skills/workflow-cli/SKILL.md', '# Skill\n\n[manual](../../docs/guide.md)')
        with self.assertRaises(ValueError):
            copy_resources(self.root, self.root.parent / 'package')

    def book_fixture(self, legacy='guide'):
        return {'title': {'en': 'Book', 'zh': '书'}, 'parts': [
            {'directory': '01-start', 'title': {'en': 'Start', 'zh': '起步'},
             'intro': {'en': 'Start here.', 'zh': '从这里开始。'}, 'chapters': [
                 {'file': f'01-start/01-{legacy}.md', 'legacy': legacy,
                  'title': {'en': 'Guide', 'zh': '指南'}}]}]}

    def test_book_rejects_missing_translation_unlisted_chapter_and_missing_volume_index(self):
        self.write('docs/book.json', json.dumps(self.book_fixture()))
        self.write('docs/en/01-start/01-guide.md', '# Guide\n\n## Run\n\nRead this guide.\n')
        with self.assertRaisesRegex(ValueError, 'Missing zh chapter'):
            sync(self.root)
        self.write('docs/zh/01-start/01-guide.md', '# 指南\n\n## 运行\n\n阅读指南。\n')
        sync(self.root)
        sync(self.root, check=True)
        cover = self.root / 'docs/en/01-start/README.md'
        self.assertIn('[1.1 Guide](01-guide.md)', cover.read_text())
        cover.unlink()
        with self.assertRaisesRegex(ValueError, 'Outdated book contents'):
            sync(self.root, check=True)
        sync(self.root)
        self.write('docs/en/01-start/02-forgotten.md', '# Forgotten')
        with self.assertRaisesRegex(ValueError, 'inventory differs'):
            sync(self.root, check=True)

    def test_book_rejects_number_gaps_wrong_groups_and_duplicate_aliases(self):
        for invalid in ['01-start/02-guide.md', '02-other/01-guide.md', '../01-guide.md',
                        '01-start/01-Guide.md', '01-start/1-guide.md']:
            book = self.book_fixture()
            book['parts'][0]['chapters'][0]['file'] = invalid
            self.write('docs/book.json', json.dumps(book))
            with self.subTest(path=invalid), self.assertRaisesRegex(ValueError, 'chapter numbering'):
                load_book(self.root)
        book = self.book_fixture()
        book['parts'][0]['directory'] = '02-start'
        self.write('docs/book.json', json.dumps(book))
        with self.assertRaisesRegex(ValueError, 'part numbering'):
            load_book(self.root)
        book = self.book_fixture()
        book['parts'][0]['chapters'].append({'file': '01-start/02-next.md', 'legacy': 'guide',
                                           'title': {'en': 'Next', 'zh': '下一章'}})
        self.write('docs/book.json', json.dumps(book))
        with self.assertRaisesRegex(ValueError, 'duplicate legacy'):
            load_book(self.root)

    def test_skill_resource_check_rejects_stale_copies(self):
        self.write('docs/en/guide.md', '# Guide')
        self.write('README.md', '# Overview')
        self.write('README.zh-CN.md', '# 概述')
        self.write('LICENSE', 'MIT')
        self.write('skills/workflow-cli/SKILL.md', '# Skill')
        sync_resources(self.root)
        sync_resources(self.root, check=True)
        self.write('docs/en/guide.md', '# Updated guide')
        with self.assertRaisesRegex(ValueError, 'Stale or missing skill resource'):
            sync_resources(self.root, check=True)
        sync_resources(self.root)
        (self.root / 'docs/en/guide.md').unlink()
        with self.assertRaisesRegex(ValueError, 'Unexpected generated'):
            sync_resources(self.root, check=True)

    def test_site_checks_fragments_and_local_resources(self):
        self.write('index.html', '<a href="zh/guide.html#运行">中文</a>')
        chapter = self.write('zh/guide.html', '<h1 id="运行">运行</h1><a href="../schema.json">schema</a>')
        self.write('schema.json', '{}')
        self.assertEqual(check_site(self.root), 2)
        chapter.write_text('<h1 id="different">章节</h1>')
        with self.assertRaisesRegex(ValueError, 'Missing site anchor'):
            check_site(self.root)

    def test_site_rejects_missing_resources_and_duplicate_ids(self):
        page = self.write('index.html', '<a href="missing.json">schema</a>')
        with self.assertRaises(ValueError):
            check_site(self.root)
        page.write_text('<h1 id="same">One</h1><h2 id="same">Two</h2>')
        with self.assertRaisesRegex(ValueError, 'Duplicate HTML id'):
            check_site(self.root)

    def test_built_book_switches_same_chapter_and_keeps_reading_assets_local(self):
        book = self.book_fixture('skill-distribution')
        self.write('docs/book.json', json.dumps(book))
        for language, heading in [('en', 'Install'), ('zh', '安装')]:
            self.write(f'docs/{language}/01-start/01-skill-distribution.md',
                       f'# {heading}\n\n## Steps\n\n' + 'Read the complete instructions. ' * 10 +
                       '\n\n[example](../../../examples/input.json)\n')
        self.write('README.md', '# Overview\n\n[book](docs/README.md)')
        self.write('README.zh-CN.md', '# 介绍\n\n[书](docs/zh/README.md)')
        self.write('skills/workflow-cli/SKILL.md', '---\nname: example\n---\n# Skill')
        self.write('examples/input.json', '{"local":true}')
        self.write('Cargo.toml', '[workspace.package]\nversion = "1.2.3"\n')
        for name in ['index.html', 'index.en.html']:
            self.write('website/' + name, '<p>v__VERSION__</p>')
        for name in ['style.css', 'site.js', 'redirect.js']:
            self.write('website/' + name, '')
        self.write('LICENSE', 'MIT')
        sync(self.root)
        sync_resources(self.root)
        output = self.root.parent / 'site'
        with patch('build_site.subprocess.check_output', return_value='a' * 40 + '\n'):
            build(output, self.root)
        en = (output / 'docs/en/01-start/01-skill-distribution.html').read_text()
        zh = (output / 'docs/zh/01-start/01-skill-distribution.html').read_text()
        self.assertIn('href="../../zh/01-start/01-skill-distribution.html"', en)
        self.assertIn('href="../../en/01-start/01-skill-distribution.html"', zh)
        self.assertNotIn('book-navigation', en)
        self.assertNotIn('name: example', (output / 'skill/index.html').read_text())
        self.assertEqual((output / 'resources/examples/input.json').read_text(), '{"local":true}')
        self.assertIn('href="../../../resources/examples/input.json"', en)
        self.assertIn('1.1 Guide', en)
        self.assertIn('href="index.html"', en)
        self.assertIn('href="en/01-start/01-skill-distribution.html"',
                      (output / 'docs/skill-distribution.html').read_text())
        self.assertTrue((output / 'docs/en/01-start/index.html').is_file())
        self.assertIn('href="../../zh/01-start/index.html"',
                      (output / 'docs/en/01-start/index.html').read_text())
        self.root.rename(self.root.parent / 'removed-source')
        self.assertGreater(check_site(output), 0)


if __name__ == '__main__':
    unittest.main()
