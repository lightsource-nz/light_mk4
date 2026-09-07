# light_presets
 CMake configuration and build descriptors for projects based on the light framework

 This repository is intended to be included as a submodule into application and library projects based on the 'light' software framework, to provide the necessary CMake configuration descriptors to be inherited by the project's own configuration descriptors. It was necessary for this object to be in its own submodule, because CMake preset files cannot be transparently provided to projects in the way that all other CMake assets for the framework are shared (through a path variable pointing to the framework source directory).

 This repository is part of the light framework project.
