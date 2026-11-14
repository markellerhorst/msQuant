added:
- rewrite code with comments and documentation according to the Google Style Guide https://google.github.io/styleguide/, specifically the Google Rust Style Guide https://google.github.io/styleguide/rustguide.html
- link the style guide in the readme
- add img/app_screenshot.png (below #Features) and img/lcms-quant-ico.png (top below the heading) to the readme
- fix that negative exponents of numbers are currently displayed with a square
- the calibration split is constantly updating and scaling when the mouse hovers anywhere over the lcms-quant window (it seems to be scaling outwards)
- rename the application to msQuant and version number to v0.1.0
- i noticed that temporary files are stored in %tmp% (C:\Users\eller\AppData\Local\Temp) - let the user decide a standard directory for all files, store every file of the project there unless the user decides for a different custom path; the selection of the standard path is a new menu point in the preferences menu (the default path when no standard path is provided is ./msQuant-default located and if required made in the root folder of the msQuant installation)
- do not store unnecessary log data, only the audit trail and user settings are to be stored when normally opening and closing the program (additionally any temporary files that you need for the program to run - all located in the ./msQuant-default folder)

add/fix the following:
- move all tests to the ./tests folder suit with a sensible file structure
- include in readme that the Google Rust Style Guide is to be followed and that where possible external libraries are to be avoided and replaced by clean re-implementations of the required functions
- add to readme where to find an analyte target example table and show an example table with an explanation for the requried columns and information
- include ci/cd test suit results live in readme if possible
- no general audit log file but one per .quant file or if no file is saved then per session (one session = open program -> work -> close program)
- make sure that cargo build --release does not include the thermorawfileparser
- check the entire code for security issues and exploits and fix if any are found
- change the github repo name to https://github.com/markellerhorst/msQuant and the ico name to ./img/msQuant-ico.ico

gui design:
- replace process main tab with the calibration file selection and target table window and dont add a process menu button
- for the "view" tab
- f