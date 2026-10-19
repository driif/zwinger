---
name: writing-simple-code
description: Use when you write or change React/TypeScript in src/. Use it before you add a prop, generic, useMemo/useCallback, helper, runtime guard, comment or CSS rule.
---

# Writing simple React/TS

Write the minimum code that does the task. Get UI elements from motiv (`building-ui-with-motiv`).

1. **Add a prop only when a caller uses it.** If all callers use the default value, put the value in the code.
   Apply the same rule to `variant`/`showX` props with one value and to generics with one concrete type.
2. **Do not repeat library defaults** in option objects. Read the library documentation.
3. **Memoize only for a consumer.** A consumer is a `[deps]` array, a memoized child, or an effect that must not run again.
   When there is a consumer, memoize.
4. **Keep code inline.** Keep a helper inline when only one location calls it.
   Extract logic when the third identical copy occurs.
5. **Use one return with a conditional child** when the branches have the same wrapper.
   Put identical `case` labels together.
6. **Trust the types.** Do not add guards or `?.` for states that the types do not allow.
7. **Write a comment only** for a subtle mechanism or an external constraint. Write no comments by default.
8. **Do not prepare for possible future features.** Do not add TODOs. Do not add CSS for features that do not exist.
9. **Rename at the source**, not in each consumer. Use an alias when two names are the same.
10. **Call Tauri through typed wrappers** around `invoke` in one module.
    Do not put command strings directly in components.

When you keep code that a rule tells you to remove, add one line that gives the reason.
