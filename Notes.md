# Plotter Max

An app that can read a file and plot it on screen. 

## App Goals
- The file format can be specified in a config file (thinkin JSON).
- The file will be read according to its format and an index of each unique data type specified in the config file will be stored.
- The gui will have a working buffer queue for each unit data type. This buffer queue will hold data that will be plotted in a later step.
- The gui will support rendering the data in a visual object called a view. This view will render a graph using the underlying buffer queue and its specified parameters. For example each buffer might be specified to hold 1000 samples.
- The app will load data from the file into the buffer queue to be available for rendering in a seamless manner as the user scrolls through the onscreen data.



## Future dreams

- Data Server for live streaming to the GUI for real-time visualization
- Load data from a URL
- Multiple data types in a single view
- Cross probing tool for highlighting viewed sample in data.
- Use cross probing tool for real time modifications to the file (removing/adding sections)
- Advanced transformations for unique views on data (head maps, Lissajous curve, stochastic distributions)


## TODO
- File parser
    - Open a file and its config (or use a default config from a menu)
    - Create a
- Buffer queue
    - used to store samples to be plotted
- Data view
    - glcanvas that plots lines and visuals