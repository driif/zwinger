import { execFileSync } from "node:child_process";
import ts from "typescript";

const git = (...args) => execFileSync("git", args, { encoding: "utf8" });
const [base, head] = process.argv.slice(2);
const start = git("merge-base", base, head).trim();
const docs = "site/src/content/docs/";
if (git("diff", "--name-only", start, head, "--", `${docs}*.md`, `${docs}*.mdx`)) process.exit(0);

const visibleName = /^(label|title|description|placeholder|alt|aria-label|aria-description|message|error|text|hint|tooltip|notice)$/i;

function visibleStrings(node, copy) {
  if (ts.isStringLiteralLike(node)) {
    copy.push(node.text);
  } else if (ts.isTemplateExpression(node)) {
    copy.push([node.head.text, ...node.templateSpans.map((span) => span.literal.text)].join("${…}"));
  } else if (ts.isConditionalExpression(node)) {
    visibleStrings(node.whenTrue, copy);
    visibleStrings(node.whenFalse, copy);
  } else if (ts.isBinaryExpression(node)) {
    const op = node.operatorToken.kind;
    if (op === ts.SyntaxKind.QuestionQuestionToken || op === ts.SyntaxKind.BarBarToken) {
      visibleStrings(node.left, copy);
      visibleStrings(node.right, copy);
    } else if (op === ts.SyntaxKind.AmpersandAmpersandToken) {
      visibleStrings(node.right, copy);
    }
  } else if ((ts.isJsxExpression(node) || ts.isParenthesizedExpression(node)) && node.expression) {
    visibleStrings(node.expression, copy);
  }
}

function collectCopy(file, content, copy) {
  const source = ts.createSourceFile(file, content, ts.ScriptTarget.Latest);
  function visit(node) {
    if (ts.isJsxText(node)) {
      const text = node.text.trim().replace(/\s+/g, " ");
      if (text) copy.push(text);
    } else if (ts.isJsxElement(node) || ts.isJsxFragment(node)) {
      for (const child of node.children) {
        if (ts.isJsxExpression(child)) visibleStrings(child, copy);
      }
    } else if (
      (ts.isJsxAttribute(node) || ts.isPropertyAssignment(node) || ts.isVariableDeclaration(node)) &&
      visibleName.test(node.name.getText(source).replace(/^["']|["']$/g, "")) && node.initializer
    ) {
      visibleStrings(node.initializer, copy);
    } else if (ts.isNewExpression(node) && node.expression.getText(source) === "Error") {
      if (node.arguments?.[0]) visibleStrings(node.arguments[0], copy);
    } else if (ts.isCallExpression(node)) {
      const name = node.expression.getText(source);
      if (/^(action|intent)$/.test(name) && node.arguments[1]) visibleStrings(node.arguments[1], copy);
      if (/^(setError|setMessage|setNotice|alert|confirm)$/.test(name) && node.arguments[0]) visibleStrings(node.arguments[0], copy);
    } else if (ts.isReturnStatement(node) || ts.isVariableDeclaration(node) || ts.isPropertyAssignment(node)) {
      const value = ts.isReturnStatement(node) ? node.expression : node.initializer;
      if (value && ts.isStringLiteralLike(value) && /^\p{Lu}[\p{L}'-]*\s+\p{L}/u.test(value.text)) {
        copy.push(value.text);
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
}

// Each side supplies its own paths, so a rename, deletion or addition needs no special case.
function changedCopy(from, to) {
  const paths = git("diff", "--no-renames", "--diff-filter=AMT", "--name-only", "-z", from, to, "--", "src/");
  const copy = [];
  for (const file of paths.split("\0")) {
    if (/\.tsx?$/.test(file) && !/\.(test|spec|d)\.tsx?$/.test(file)) {
      collectCopy(file, git("show", `${to}:${file}`), copy);
    }
  }
  return copy.sort();
}

if (JSON.stringify(changedCopy(start, head)) !== JSON.stringify(changedCopy(head, start))) {
  console.log("::warning title=Check user documentation::App text changed. Check whether a page in site/src/content/docs/ needs an update.");
}
