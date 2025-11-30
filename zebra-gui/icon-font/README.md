
# How To Update The Icon Font File With New Icons

1. Go to [Fontello](https://fontello.com/). Click the wrench in the top right > "Import", and select the `config.json` in this repo. This way you can modify the existing icon set instead of starting fresh.
2. Select the new icons you want and deselect any icons you don't want. Click "Download webfont" when you're done, and save the zip as `fontello.zip` (not `fontello-<id>.zip`).
3. Overwrite the `fontello.zip` in the repo and extract the entire zip file. There should be a `fontello-<id>` folder in the directory now. This folder contains all the new data, but it is temporary and we will delete it soon.
4. **Drag and drop** the folder onto the **`gen_icons.bat`** script in the directory. This will automatically copy out the new `config.json` file, **sort it** (which is important so we can make the git diffs readable and vettable), and auto-generate the new C header for the icons.
5. **Delete the `fontello-<id>` folder** so that it doesn't pollute the directory or the Git diff.
6. **Vet your Git diff** to ensure that the only changes are: the modified `fontello.zip`, the modified `fontello.ttf`, and **small, localized text diffs** of added & removed glyphs in `config.json` and `fontello_icons.h`.
7. Ship it!

---

### (Old Instructions - No Automation Script)

> 1. Go to [Fontello](https://fontello.com/). Click the wrench in the top right > "Import", and select the `config.json` in this repo. This way you can modify the existing icon set instead of starting fresh.
> 2. Select the new icons you want and deselect any icons you don't want. Click "Download webfont" when you're done, and save the zip as `fontello.zip` (not `fontello-<id>.zip`).
> 3. Overwrite the `fontello.zip` in the repo and extract the new `fontello.ttf` from `fontello.zip/fontello-<id>/font/` and the new `config.json` from `fontello.zip/fontello-<id>/`.
> 4. In the new JSON file `config.json`, copy out the *array* (named `"glyphs"`) and paste it into [this online JSON sorter](https://codeshack.io/json-sorter/) (or any other JSON sorter that you've confirmed actually works). In **Sort Method**, select **Key Value**, and in **Key Name**, type in **`code`**. This will sort the glyphs by codepoint in the JSON, which is important so we can make the git diffs readable and vettable.
> 5. Copy the JSON in the **Output text** field back into the `config.json` file, replacing the old `glyphs` array with your new sorted array. **Fix the whitespace!!** Make sure you're using the same number of spaces for indentation, everywhere, that it had before.
> 6. **Vet your Git diff** to see that the only changes are: the modified `fontello.zip`, the modified `fontello.ttf`, and **small, localized text diffs** of added & removed glyphs in `config.json`.
> 7. Ship it!
